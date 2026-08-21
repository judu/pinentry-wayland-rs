//! A pulsing border around every output while the PIN dialog is up.
//!
//! Ported from `dbus-pulse-niri`: one `wlr-layer-shell` overlay surface per
//! output, anchored to all four edges with `exclusive_zone = -1`, so it covers
//! the whole output above every window. An empty input region makes it
//! click-through, and no keyboard interactivity keeps focus on the dialog.
//!
//! Only the edge strip is ever painted or damaged; the interior of the buffer
//! stays transparent for the whole life of the surface, which is why each
//! surface gets its own pool with two dedicated slots (a fresh pool is zeroed
//! and nobody else can be handed the same memory).

use std::time::Instant;

use smithay_client_toolkit::compositor::{CompositorState, Region};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerSurface,
};
use smithay_client_toolkit::shm::Shm;
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use wayland_client::protocol::{wl_output, wl_shm, wl_surface};
use wayland_client::QueueHandle;

use crate::wayland_window::PinEntryWindow;

type Qh = QueueHandle<PinEntryWindow>;

/// Border color: a blue-green, straight (non-premultiplied) RGBA in 0..=1.
const COLOR: [f32; 4] = [0.25, 0.80, 0.70, 0.95];
/// Border thickness in logical pixels.
const THICKNESS: u32 = 8;
/// One fade half-cycle (bright to dim, or dim to bright) in milliseconds.
const HALF_PERIOD_MS: f64 = 700.0;
/// Opacity floor of the pulse.
const OPACITY_MIN: f32 = 60.0 / 255.0;

/// A horizontal run of pixels to paint, in buffer coordinates.
#[derive(Clone, Copy, Debug)]
struct Span {
    y: u32,
    x0: u32,
    x1: u32,
}

/// A rectangle of the buffer that changes every frame, for `damage_buffer`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

/// Buffer size plus the thickness of the painted edge strip, in device pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Geometry {
    width: u32,
    height: u32,
    strip: u32,
}

/// Split the edge strip into the four rectangles that tile it without overlap:
/// full-width bands top and bottom, and the two sides between them.
fn edge_rects(g: Geometry) -> Vec<Rect> {
    let (w, h, t) = (g.width as i32, g.height as i32, g.strip as i32);
    // On an output too small to hold two bands, the strip is the whole surface.
    if 2 * t >= h || 2 * t >= w {
        return vec![Rect { x: 0, y: 0, width: w, height: h }];
    }
    vec![
        Rect { x: 0, y: 0, width: w, height: t },
        Rect { x: 0, y: h - t, width: w, height: t },
        Rect { x: 0, y: t, width: t, height: h - 2 * t },
        Rect { x: w - t, y: t, width: t, height: h - 2 * t },
    ]
}

/// The same tiling, expressed as scanline runs.
fn edge_spans(g: Geometry) -> Vec<Span> {
    let (w, h, t) = (g.width, g.height, g.strip);
    let mut spans = Vec::with_capacity(h as usize + 2 * t as usize);
    if 2 * t >= h || 2 * t >= w {
        spans.extend((0..h).map(|y| Span { y, x0: 0, x1: w }));
        return spans;
    }
    spans.extend((0..t).map(|y| Span { y, x0: 0, x1: w }));
    for y in t..h - t {
        spans.push(Span { y, x0: 0, x1: t });
        spans.push(Span { y, x0: w - t, x1: w });
    }
    spans.extend((h - t..h).map(|y| Span { y, x0: 0, x1: w }));
    spans
}

/// Pack a straight color and a total alpha into a premultiplied ARGB8888 pixel.
#[inline]
fn premultiplied(r: f32, g: f32, b: f32, alpha: f32) -> [u8; 4] {
    let a = alpha.clamp(0.0, 1.0);
    let q = |c: f32| (c.clamp(0.0, 1.0) * a * 255.0 + 0.5) as u8;
    // ARGB8888 in native (little-endian) byte order is B, G, R, A.
    [q(b), q(g), q(r), (a * 255.0 + 0.5) as u8]
}

fn ease_in_out_quad(x: f32) -> f32 {
    if x < 0.5 { 2.0 * x * x } else { 1.0 - (-2.0 * x + 2.0).powi(2) / 2.0 }
}

/// Opacity eased from max to min over one half period, then back, forever.
/// Read off the clock so every output stays in step even if one drops a frame.
fn pulse_opacity(elapsed_ms: f64, half_period_ms: f64) -> f32 {
    if half_period_ms <= 0.0 {
        return 1.0;
    }
    let u = (elapsed_ms.rem_euclid(2.0 * half_period_ms) / half_period_ms) as f32;
    if u < 1.0 {
        1.0 + (OPACITY_MIN - 1.0) * ease_in_out_quad(u)
    } else {
        OPACITY_MIN + (1.0 - OPACITY_MIN) * ease_in_out_quad(u - 1.0)
    }
}

fn paint(geom: Geometry, canvas: &mut [u8], elapsed_ms: f64) {
    let [r, g, b, a] = COLOR;
    let pixel = premultiplied(r, g, b, a * pulse_opacity(elapsed_ms, HALF_PERIOD_MS));

    let stride = geom.width as usize * 4;
    for span in edge_spans(geom) {
        let start = span.y as usize * stride + span.x0 as usize * 4;
        let end = span.y as usize * stride + span.x1 as usize * 4;
        for chunk in canvas[start..end].chunks_exact_mut(4) {
            chunk.copy_from_slice(&pixel);
        }
    }
}

/// One overlay surface on one output.
struct Surface {
    output: wl_output::WlOutput,
    layer: LayerSurface,
    /// Kept alive so the compositor's copy of the (empty) input region is not
    /// racing our destroy request.
    _input_region: Region,
    pool: Option<SlotPool>,
    /// Exactly two, so a frame can always be painted into the one the
    /// compositor is not reading.
    buffers: Vec<Buffer>,
    geom: Option<Geometry>,
    logical: (u32, u32),
    scale: i32,
    configured: bool,
    first_commit: bool,
    frame_pending: bool,
}

impl Surface {
    fn draw(&mut self, shm: &Shm, qh: &Qh, elapsed_ms: f64) {
        if !self.configured {
            return;
        }
        let scale = self.scale.max(1);
        let width = self.logical.0 * scale as u32;
        let height = self.logical.1 * scale as u32;
        if width == 0 || height == 0 {
            return;
        }

        if self.geom.map(|g| (g.width, g.height)) != Some((width, height)) {
            if let Err(e) = self.reallocate(shm, width, height, scale) {
                log::error!("ring: failed to allocate a {width}x{height} buffer: {e}");
                return;
            }
        }

        let Surface { pool, buffers, geom, layer, first_commit, frame_pending, .. } = self;
        let (Some(pool), Some(geom)) = (pool.as_mut(), *geom) else { return };
        let surface = layer.wl_surface();

        // Both buffers still in the compositor's hands: skip this frame rather
        // than tearing, and ask to be woken again.
        let Some(index) = buffers.iter().position(|b| b.canvas(pool).is_some()) else {
            surface.frame(qh, surface.clone());
            *frame_pending = true;
            layer.commit();
            return;
        };

        let canvas = buffers[index].canvas(pool).expect("just checked");
        paint(geom, canvas, elapsed_ms);

        if *first_commit {
            surface.damage_buffer(0, 0, width as i32, height as i32);
            *first_commit = false;
        } else {
            for r in edge_rects(geom) {
                surface.damage_buffer(r.x, r.y, r.width, r.height);
            }
        }

        surface.frame(qh, surface.clone());
        *frame_pending = true;

        if let Err(e) = buffers[index].attach_to(surface) {
            log::error!("ring: failed to attach a buffer: {e}");
            return;
        }
        layer.commit();
    }

    fn reallocate(&mut self, shm: &Shm, width: u32, height: u32, scale: i32) -> Result<(), String> {
        let stride = width as i32 * 4;
        let len = height as usize * stride as usize;

        // Two dedicated slots rather than `SlotPool::create_buffer`: we only
        // ever write the edge strip and rely on the rest of the buffer staying
        // transparent, which holds only if no other surface can be handed the
        // same memory. A freshly mapped pool is zeroed.
        let mut pool = SlotPool::new(len * 2 + 128, shm).map_err(|e| format!("creating an shm pool: {e}"))?;
        let mut buffers = Vec::with_capacity(2);
        for _ in 0..2 {
            let slot = pool.new_slot(len).map_err(|e| format!("allocating an shm slot: {e}"))?;
            buffers.push(
                pool.create_buffer_in(&slot, width as i32, height as i32, stride, wl_shm::Format::Argb8888)
                    .map_err(|e| format!("creating a wl_buffer: {e}"))?,
            );
        }

        let strip = (THICKNESS * scale.max(1) as u32).max(1).min(width.min(height).max(1));
        self.geom = Some(Geometry { width, height, strip });
        self.pool = Some(pool);
        self.buffers = buffers;
        self.first_commit = true;
        self.layer.wl_surface().set_buffer_scale(scale);
        Ok(())
    }
}

/// The ring across every output. Owned by the window state; the window's
/// handlers forward layer-shell, output and frame events here.
pub struct Ring {
    layer_shell: Option<LayerShell>,
    surfaces: Vec<Surface>,
    /// Animation clock, shared by every output so they stay in step.
    start: Instant,
}

impl Ring {
    /// `layer_shell` is `None` when the compositor has no `zwlr_layer_shell_v1`;
    /// the ring is then silently disabled.
    pub fn new(layer_shell: Option<LayerShell>) -> Self {
        Self { layer_shell, surfaces: Vec::new(), start: Instant::now() }
    }

    fn elapsed_ms(&self) -> f64 {
        self.start.elapsed().as_secs_f64() * 1000.0
    }

    /// Does this surface belong to the ring?
    pub fn owns(&self, surface: &wl_surface::WlSurface) -> bool {
        self.index_of(surface).is_some()
    }

    fn index_of(&self, surface: &wl_surface::WlSurface) -> Option<usize> {
        self.surfaces.iter().position(|s| s.layer.wl_surface() == surface)
    }

    /// Light up `output`. No-op if it is already lit or layer-shell is absent.
    pub fn add_output(
        &mut self,
        qh: &Qh,
        compositor: &CompositorState,
        output: wl_output::WlOutput,
        scale: i32,
    ) {
        let Some(layer_shell) = self.layer_shell.as_ref() else { return };
        if self.surfaces.iter().any(|s| s.output == output) {
            return;
        }

        let wl_surface = compositor.create_surface(qh);
        let layer = layer_shell.create_layer_surface(
            qh,
            wl_surface,
            Layer::Overlay,
            Some("pinentry-ring"),
            Some(&output),
        );

        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_size(0, 0);
        // -1, not 0: cover the whole output, ignoring panels and other
        // exclusive zones, and never reserve space of our own.
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_margin(0, 0, 0, 0);

        let region = match Region::new(compositor) {
            Ok(region) => region,
            Err(e) => {
                log::error!("ring: could not create an empty input region: {e}");
                return;
            }
        };
        // An empty region means every click, tap and hover passes through.
        layer.wl_surface().set_input_region(Some(region.wl_region()));

        // The first commit must carry no buffer; the compositor answers with a
        // configure that tells us how big the surface really is.
        layer.commit();

        self.surfaces.push(Surface {
            output,
            layer,
            _input_region: region,
            pool: None,
            buffers: Vec::new(),
            geom: None,
            logical: (0, 0),
            scale: scale.max(1),
            configured: false,
            first_commit: true,
            frame_pending: false,
        });
    }

    pub fn remove_output(&mut self, output: &wl_output::WlOutput) {
        self.surfaces.retain(|s| &s.output != output);
    }

    pub fn closed(&mut self, layer: &LayerSurface) {
        self.surfaces.retain(|s| &s.layer != layer);
    }

    pub fn configure(&mut self, shm: &Shm, qh: &Qh, layer: &LayerSurface, new_size: (u32, u32)) {
        let Some(index) = self.surfaces.iter().position(|s| &s.layer == layer) else { return };
        let surf = &mut self.surfaces[index];
        surf.logical = new_size;
        let first = !surf.configured;
        surf.configured = true;

        // Only kick the render loop when nothing is already driving it, or a
        // mid-animation reconfigure would double the frame rate.
        if first || !surf.frame_pending {
            self.draw(shm, qh, index);
        }
    }

    pub fn scale_factor_changed(
        &mut self,
        shm: &Shm,
        qh: &Qh,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        let Some(index) = self.index_of(surface) else { return };
        self.surfaces[index].scale = new_factor.max(1);
        // The next frame callback reallocates at the new size; drawing here
        // would start a second callback chain.
        if !self.surfaces[index].frame_pending {
            self.draw(shm, qh, index);
        }
    }

    pub fn frame(&mut self, shm: &Shm, qh: &Qh, surface: &wl_surface::WlSurface) {
        let Some(index) = self.index_of(surface) else { return };
        self.surfaces[index].frame_pending = false;
        self.draw(shm, qh, index);
    }

    fn draw(&mut self, shm: &Shm, qh: &Qh, index: usize) {
        let elapsed = self.elapsed_ms();
        if let Some(surf) = self.surfaces.get_mut(index) {
            surf.draw(shm, qh, elapsed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulse_endpoints() {
        assert!((pulse_opacity(0.0, 700.0) - 1.0).abs() < 1e-6);
        assert!((pulse_opacity(700.0, 700.0) - OPACITY_MIN).abs() < 1e-6);
        assert!((pulse_opacity(1400.0, 700.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn edge_rects_tile_the_strip_without_overlap() {
        let g = Geometry { width: 100, height: 60, strip: 10 };
        let area: i32 = edge_rects(g).iter().map(|r| r.width * r.height).sum();
        let spanned: u32 = edge_spans(g).iter().map(|s| s.x1 - s.x0).sum();
        assert_eq!(area as u32, spanned);
        assert_eq!(area, 100 * 60 - 80 * 40);
    }

    #[test]
    fn paints_only_the_border() {
        let g = Geometry { width: 10, height: 10, strip: 2 };
        let mut canvas = vec![0u8; 10 * 10 * 4];
        paint(g, &mut canvas, 0.0);
        let px = |x: usize, y: usize| canvas[(y * 10 + x) * 4 + 3];
        assert!(px(0, 0) > 0);
        assert!(px(1, 5) > 0);
        assert_eq!(px(2, 5), 0, "the interior must stay untouched");
        assert!(px(9, 9) > 0);
    }
}
