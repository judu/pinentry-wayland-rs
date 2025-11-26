use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    data_device_manager::{
        data_device::{DataDevice, DataDeviceHandler},
        data_offer::{DataOfferHandler, DragOffer, SelectionOffer},
        data_source::DataSourceHandler,
        DataDeviceManagerState, WritePipe,
    },
    delegate_compositor, delegate_data_device, delegate_keyboard, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm, delegate_xdg_shell, delegate_xdg_window,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{PointerEvent, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        xdg::{
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
            XdgShell,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use std::io::Read;
use std::sync::{Arc, Mutex};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface},
    Connection, EventQueue, QueueHandle,
};
use swash::{
    FontRef,
    shape::ShapeContext,
    scale::{ScaleContext, Render, Source, StrikeWith, image::Content},
    text::Script,
    zeno::Format,
};

const WINDOW_WIDTH: u32 = 400;
const WINDOW_HEIGHT: u32 = 200;

fn load_system_font() -> Vec<u8> {
    // Try to load a common system font
    let font_paths = [
        "/usr/share/fonts/X11/dejavu/DejaVuSans.ttf",
    ];

    for path in &font_paths {
        if let Ok(data) = std::fs::read(path) {
            log::debug!("Loaded font from: {}", path);
            return data;
        }
    }

    log::debug!("Failed to load any system font, using fallback");
    panic!("No system font found. Please install DejaVu Sans or Liberation Sans fonts.");
}

pub struct PinEntryWindow {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor_state: CompositorState,
    shm_state: Shm,
    xdg_shell_state: XdgShell,
    data_device_manager_state: DataDeviceManagerState,

    window: Option<Window>,
    pool: Option<SlotPool>,
    data_device: Option<DataDevice>,
    width: u32,
    height: u32,

    description: String,
    prompt: String,
    title: String,
    pin_input: String,
    result: Arc<Mutex<Option<Result<String, String>>>>,
    cursor_visible: bool,
    configured: bool,
    modifiers: Modifiers,
    clipboard_offer: Option<SelectionOffer>,
    clipboard_content: Arc<Mutex<Option<String>>>,

    font_data: Vec<u8>,
    shape_context: ShapeContext,
    scale_context: ScaleContext,
}

impl PinEntryWindow {
    pub fn new(description: String, prompt: String, title: String) -> (Self, Connection, EventQueue<Self>) {
        let conn = Connection::connect_to_env().expect("Failed to connect to Wayland");
        let (globals, event_queue) = registry_queue_init(&conn).expect("Failed to init registry");
        let qh = event_queue.handle();

        let registry_state = RegistryState::new(&globals);
        let seat_state = SeatState::new(&globals, &qh);
        let output_state = OutputState::new(&globals, &qh);
        let compositor_state = CompositorState::bind(&globals, &qh)
            .expect("wl_compositor not available");
        let shm_state = Shm::bind(&globals, &qh).expect("wl_shm not available");
        let xdg_shell_state = XdgShell::bind(&globals, &qh).expect("xdg_shell not available");
        let data_device_manager_state = DataDeviceManagerState::bind(&globals, &qh)
            .expect("wl_data_device_manager not available");

        let font_data = load_system_font();

        let app = Self {
            registry_state,
            seat_state,
            output_state,
            compositor_state,
            shm_state,
            xdg_shell_state,
            data_device_manager_state,
            window: None,
            pool: None,
            data_device: None,
            width: WINDOW_WIDTH,
            height: WINDOW_HEIGHT,
            description,
            prompt,
            title,
            pin_input: String::new(),
            result: Arc::new(Mutex::new(None)),
            cursor_visible: true,
            configured: false,
            modifiers: Modifiers::default(),
            clipboard_offer: None,
            clipboard_content: Arc::new(Mutex::new(None)),
            font_data,
            shape_context: ShapeContext::new(),
            scale_context: ScaleContext::new(),
        };

        (app, conn, event_queue)
    }

    pub fn create_window(&mut self, qh: &QueueHandle<Self>) {
        let surface = self.compositor_state.create_surface(qh);
        let window = self.xdg_shell_state.create_window(
            surface,
            WindowDecorations::ServerDefault,
            qh,
        );

        window.set_title(&self.title);
        window.set_app_id("pinentry-wayland");
        window.set_min_size(Some((WINDOW_WIDTH, WINDOW_HEIGHT)));
        window.commit();

        self.window = Some(window);

        let pool = SlotPool::new(
            (self.width * self.height * 4) as usize,
            &self.shm_state,
        ).expect("Failed to create pool");
        self.pool = Some(pool);
    }

    pub fn draw(&mut self, _qh: &QueueHandle<Self>) {
        if !self.configured {
            return;
        }

        let window = match self.window.as_ref() {
            Some(w) => w,
            None => return,
        };

        let stride = self.width as i32 * 4;
        let width = self.width;
        let height = self.height;

        // Get mutable references to data we need
        let font_data_ptr = self.font_data.as_ptr();
        let font_data_len = self.font_data.len();
        let pin_input_len = self.pin_input.len();
        let cursor_visible = self.cursor_visible;
        let description = self.description.clone();
        let prompt = self.prompt.clone();

        let pool = match self.pool.as_mut() {
            Some(p) => p,
            None => return,
        };

        let (buffer, canvas) = pool
            .create_buffer(
                width as i32,
                height as i32,
                stride,
                wayland_client::protocol::wl_shm::Format::Argb8888,
            )
            .expect("Failed to create buffer");

        // Create a temporary font data slice for rendering
        let font_data = unsafe { std::slice::from_raw_parts(font_data_ptr, font_data_len) };

        Self::render_to_canvas(
            canvas,
            width,
            height,
            font_data,
            &mut self.shape_context,
            &mut self.scale_context,
            pin_input_len,
            cursor_visible,
            &description,
            &prompt,
        );

        window
            .wl_surface()
            .damage_buffer(0, 0, width as i32, height as i32);
        window.wl_surface().attach(Some(buffer.wl_buffer()), 0, 0);
        window.wl_surface().commit();
    }

    fn render_to_canvas(
        canvas: &mut [u8],
        width: u32,
        _height: u32,
        font_data: &[u8],
        shape_context: &mut ShapeContext,
        scale_context: &mut ScaleContext,
        pin_input_len: usize,
        cursor_visible: bool,
        description: &str,
        prompt: &str,
    ) {
        let bg_color = 0xFF1E1E2Eu32;
        let text_area_color = 0xFF313244u32;
        let text_color = 0xFFB4BEFEu32;
        let label_color = 0xFFB4BEFEu32;
        let cursor_color = 0xFFBAC2DEu32;

        for pixel in canvas.chunks_exact_mut(4) {
            pixel.copy_from_slice(&bg_color.to_ne_bytes());
        }

        Self::draw_text_with_font(canvas, width, description, 20.0, 40.0, 14.0, label_color, font_data, shape_context, scale_context);
        Self::draw_text_with_font(canvas, width, prompt, 20.0, 115.0, 14.0, label_color, font_data, shape_context, scale_context);

        let input_box_y = 120;
        let input_box_height = 40;
        let padding = 20;

        for y in input_box_y..(input_box_y + input_box_height) {
            for x in padding..(width - padding) {
                let offset = ((y * width + x) * 4) as usize;
                if offset + 4 <= canvas.len() {
                    canvas[offset..offset + 4].copy_from_slice(&text_area_color.to_ne_bytes());
                }
            }
        }

        let asterisk_width = 8;
        let asterisk_height = 8;
        let start_x = padding + 10;
        let start_y = input_box_y + 16;

        for i in 0..pin_input_len {
            let asterisk_x = start_x + (i as u32 * (asterisk_width + 4));

            for dy in 0..asterisk_height {
                for dx in 0..asterisk_width {
                    let should_draw = match (dx, dy) {
                        (3..=4, _) => true,
                        (_, 3..=4) => true,
                        (2, 2) | (5, 2) | (2, 5) | (5, 5) => true,
                        (1, 1) | (6, 1) | (1, 6) | (6, 6) => true,
                        _ => false,
                    };

                    if should_draw {
                        let x = asterisk_x + dx;
                        let y = start_y + dy;
                        let offset = ((y * width + x) * 4) as usize;
                        if offset + 4 <= canvas.len() {
                            canvas[offset..offset + 4].copy_from_slice(&text_color.to_ne_bytes());
                        }
                    }
                }
            }
        }

        if cursor_visible {
            let cursor_x = start_x + (pin_input_len as u32 * (asterisk_width + 4));
            for y in (input_box_y + 10)..(input_box_y + input_box_height - 10) {
                for x in cursor_x..(cursor_x + 2) {
                    let offset = ((y * width + x) * 4) as usize;
                    if offset + 4 <= canvas.len() {
                        canvas[offset..offset + 4].copy_from_slice(&cursor_color.to_ne_bytes());
                    }
                }
            }
        }
    }

    fn draw_text_with_font(
        canvas: &mut [u8],
        width: u32,
        text: &str,
        x: f32,
        y: f32,
        font_size: f32,
        color: u32,
        font_data: &[u8],
        shape_context: &mut ShapeContext,
        scale_context: &mut ScaleContext,
    ) {
        // Create FontRef from loaded font data
        let font_ref = match FontRef::from_index(font_data, 0) {
            Some(font) => font,
            None => {
                log::debug!("Failed to create FontRef from font data");
                return;
            }
        };

        // Shape the text
        let mut shaper = shape_context
            .builder(font_ref)
            .script(Script::Latin)
            .size(font_size)
            .build();

        shaper.add_str(text);

        // Collect glyph info with their positions
        let mut glyphs = Vec::new();
        let mut x_pos = 0.0f32;
        shaper.shape_with(|cluster| {
            for glyph in cluster.glyphs {
                // glyph.x and glyph.y are offsets within the cluster, not cumulative positions
                // We need to track the cumulative x position ourselves
                glyphs.push((glyph.id, x_pos + glyph.x, glyph.y));
                x_pos += glyph.advance;
            }
        });

        // Create scaler for rendering glyphs
        let mut scaler = scale_context
            .builder(font_ref)
            .size(font_size)
            .hint(true)
            .build();

        // Render each glyph
        for (glyph_id, glyph_x, glyph_y) in glyphs {
            // Render the glyph
            let image = Render::new(&[
                Source::ColorOutline(0),
                Source::ColorBitmap(StrikeWith::BestFit),
                Source::Outline,
            ])
            .format(Format::Alpha)
            .render(&mut scaler, glyph_id);

            if let Some(image) = image {
                let glyph_data = image.data;

                // Calculate position for this glyph
                let glyph_pixel_x = (x + glyph_x).round() as i32 + image.placement.left;
                let glyph_pixel_y = (y + glyph_y).round() as i32 - image.placement.top;

                // Extract color components (color is in ARGB format)
                let alpha = ((color >> 24) & 0xFF) as u8;
                let red = ((color >> 16) & 0xFF) as u8;
                let green = ((color >> 8) & 0xFF) as u8;
                let blue = (color & 0xFF) as u8;

                // Composite the glyph onto the canvas
                match image.content {
                    Content::Mask => {
                        // Alpha mask rendering
                        for gy in 0..image.placement.height {
                            for gx in 0..image.placement.width {
                                let canvas_x = glyph_pixel_x + gx as i32;
                                let canvas_y = glyph_pixel_y + gy as i32;

                                if canvas_x < 0 || canvas_y < 0 || canvas_x >= width as i32 || canvas_y >= (canvas.len() / (width as usize * 4)) as i32 {
                                    continue;
                                }

                                let glyph_idx = (gy * image.placement.width + gx) as usize;
                                let glyph_alpha = glyph_data[glyph_idx];

                                if glyph_alpha > 0 {
                                    let canvas_offset = ((canvas_y as u32 * width + canvas_x as u32) * 4) as usize;
                                    if canvas_offset + 4 <= canvas.len() {
                                        // Alpha blending
                                        let fg_alpha = ((alpha as u16 * glyph_alpha as u16) / 255) as u8;
                                        let inv_alpha = 255 - fg_alpha;

                                        let bg_b = canvas[canvas_offset];
                                        let bg_g = canvas[canvas_offset + 1];
                                        let bg_r = canvas[canvas_offset + 2];
                                        let bg_a = canvas[canvas_offset + 3];

                                        canvas[canvas_offset] = ((blue as u16 * fg_alpha as u16 + bg_b as u16 * inv_alpha as u16) / 255) as u8;
                                        canvas[canvas_offset + 1] = ((green as u16 * fg_alpha as u16 + bg_g as u16 * inv_alpha as u16) / 255) as u8;
                                        canvas[canvas_offset + 2] = ((red as u16 * fg_alpha as u16 + bg_r as u16 * inv_alpha as u16) / 255) as u8;
                                        canvas[canvas_offset + 3] = bg_a.saturating_add(fg_alpha);
                                    }
                                }
                            }
                        }
                    }
                    Content::Color | Content::SubpixelMask => {
                        // For color glyphs or subpixel rendering, use data directly
                        // This is a simplified implementation
                        for gy in 0..image.placement.height {
                            for gx in 0..image.placement.width {
                                let canvas_x = glyph_pixel_x + gx as i32;
                                let canvas_y = glyph_pixel_y + gy as i32;

                                if canvas_x < 0 || canvas_y < 0 || canvas_x >= width as i32 || canvas_y >= (canvas.len() / (width as usize * 4)) as i32 {
                                    continue;
                                }

                                let glyph_idx = (gy * image.placement.width + gx) as usize;
                                if glyph_idx < glyph_data.len() {
                                    let glyph_alpha = glyph_data[glyph_idx];

                                    if glyph_alpha > 0 {
                                        let canvas_offset = ((canvas_y as u32 * width + canvas_x as u32) * 4) as usize;
                                        if canvas_offset + 4 <= canvas.len() {
                                            let fg_alpha = ((alpha as u16 * glyph_alpha as u16) / 255) as u8;
                                            let inv_alpha = 255 - fg_alpha;

                                            let bg_b = canvas[canvas_offset];
                                            let bg_g = canvas[canvas_offset + 1];
                                            let bg_r = canvas[canvas_offset + 2];
                                            let bg_a = canvas[canvas_offset + 3];

                                            canvas[canvas_offset] = ((blue as u16 * fg_alpha as u16 + bg_b as u16 * inv_alpha as u16) / 255) as u8;
                                            canvas[canvas_offset + 1] = ((green as u16 * fg_alpha as u16 + bg_g as u16 * inv_alpha as u16) / 255) as u8;
                                            canvas[canvas_offset + 2] = ((red as u16 * fg_alpha as u16 + bg_r as u16 * inv_alpha as u16) / 255) as u8;
                                            canvas[canvas_offset + 3] = bg_a.saturating_add(fg_alpha);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }


    fn read_clipboard(&mut self, offer: SelectionOffer) {
        // Try text/plain first
        let mime_type = if offer.with_mime_types(|types| types.contains(&"text/plain".to_string())) {
            "text/plain"
        } else if offer.with_mime_types(|types| types.contains(&"text/plain;charset=utf-8".to_string())) {
            "text/plain;charset=utf-8"
        } else if offer.with_mime_types(|types| types.contains(&"UTF8_STRING".to_string())) {
            "UTF8_STRING"
        } else if offer.with_mime_types(|types| types.contains(&"STRING".to_string())) {
            "STRING"
        } else {
            log::debug!("No supported text mime type in clipboard");
            return;
        };

        log::debug!("Reading clipboard with mime type: {}", mime_type);

        match offer.receive(mime_type.to_string()) {
            Ok(mut read_pipe) => {
                // Spawn a thread to read clipboard data to avoid blocking the event loop
                let clipboard_content = Arc::clone(&self.clipboard_content);
                std::thread::spawn(move || {
                    let mut content = String::new();
                    match read_pipe.read_to_string(&mut content) {
                        Ok(_) => {
                            log::debug!("Read {} characters from clipboard", content.len());
                            *clipboard_content.lock().unwrap() = Some(content);
                        }
                        Err(e) => {
                            log::debug!("Failed to read clipboard data: {}", e);
                        }
                    }
                });
            }
            Err(e) => {
                log::debug!("Failed to receive clipboard data: {}", e);
            }
        }
    }

    pub fn get_result(&self) -> Arc<Mutex<Option<Result<String, String>>>> {
        Arc::clone(&self.result)
    }
}

impl CompositorHandler for PinEntryWindow {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wayland_client::protocol::wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        self.draw(qh);
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for PinEntryWindow {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl WindowHandler for PinEntryWindow {
    fn request_close(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _window: &Window) {
        *self.result.lock().unwrap() = Some(Err("User cancelled".to_string()));
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _window: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        self.configured = true;

        if let (Some(width), Some(height)) = configure.new_size {
            self.width = width.get();
            self.height = height.get();

            if let Some(pool) = self.pool.as_mut() {
                pool.resize((self.width * self.height * 4) as usize)
                    .expect("Failed to resize pool");
            }
        }

        self.draw(qh);
    }
}

impl SeatHandler for PinEntryWindow {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard {
            self.seat_state.get_keyboard(qh, &seat, None).ok();

            // Create data device for clipboard access when we get keyboard capability
            if self.data_device.is_none() {
                let data_device = self.data_device_manager_state.get_data_device(qh, &seat);
                self.data_device = Some(data_device);
            }
        }
        if capability == Capability::Pointer {
            self.seat_state.get_pointer(qh, &seat).ok();
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        _capability: Capability,
    ) {
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}
}

impl KeyboardHandler for PinEntryWindow {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
        _raw: &[u32],
        _keysyms: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        let keysym = event.keysym;
        let ctrl_pressed = self.modifiers.ctrl;

        if keysym == Keysym::Return || keysym == Keysym::KP_Enter {
            *self.result.lock().unwrap() = Some(Ok(self.pin_input.clone()));
        } else if keysym == Keysym::Escape {
            *self.result.lock().unwrap() = Some(Err("User cancelled".to_string()));
        } else if keysym == Keysym::BackSpace {
            self.pin_input.pop();
            self.draw(qh);
        } else if ctrl_pressed && (keysym == Keysym::v || keysym == Keysym::V) {
            // Trigger paste from clipboard
            // First check if we have clipboard content ready from a previous read
            let clipboard_content = self.clipboard_content.lock().unwrap().take();
            if let Some(content) = clipboard_content {
                log::debug!("Pasting {} characters from clipboard", content.len());
                self.pin_input.push_str(&content);
                self.draw(qh);
            } else if let Some(offer) = self.clipboard_offer.take() {
                // Start reading clipboard asynchronously
                log::debug!("Requesting clipboard data");
                self.read_clipboard(offer);
                // The content will be available on the next Ctrl+V press
                log::debug!("Clipboard read in progress, press Ctrl+V again to paste");
            } else {
                log::debug!("No clipboard data available");
            }
        } else if ctrl_pressed && (keysym == Keysym::a || keysym == Keysym::A) {
            // Select all doesn't make sense for password fields
            log::debug!("Select all via Ctrl+A ignored (not applicable for password fields)");
        } else if let Some(c) = keysym_to_char(keysym) {
            if c.is_ascii_alphanumeric() || c.is_ascii_punctuation() || c.is_ascii_whitespace() {
                self.pin_input.push(c);
                self.draw(qh);
            }
        }
    }

    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        modifiers: Modifiers,
        _raw_modifiers: RawModifiers,
        _layout: u32,
    ) {
        self.modifiers = modifiers;
    }

    fn repeat_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
    }
}

impl PointerHandler for PinEntryWindow {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        _events: &[PointerEvent],
    ) {
    }
}

impl ShmHandler for PinEntryWindow {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm_state
    }
}

impl DataDeviceHandler for PinEntryWindow {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _data_device: &wayland_client::protocol::wl_data_device::WlDataDevice,
        _x: f64,
        _y: f64,
        _surface: &wl_surface::WlSurface,
    ) {
    }

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _data_device: &wayland_client::protocol::wl_data_device::WlDataDevice,
    ) {
    }

    fn motion(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _data_device: &wayland_client::protocol::wl_data_device::WlDataDevice,
        _x: f64,
        _y: f64,
    ) {
    }

    fn drop_performed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _data_device: &wayland_client::protocol::wl_data_device::WlDataDevice,
    ) {
    }

    fn selection(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _data_device: &wayland_client::protocol::wl_data_device::WlDataDevice,
    ) {
        log::debug!("Clipboard selection changed");
        // Get the current selection offer from the data device
        if let Some(device) = &self.data_device {
            if let Some(offer) = device.data().selection_offer() {
                log::debug!("Storing new clipboard offer");
                self.read_clipboard(offer);
                //self.clipboard_offer = Some(offer);
            }
        }
    }
}

impl DataSourceHandler for PinEntryWindow {
    fn accept_mime(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _source: &wayland_client::protocol::wl_data_source::WlDataSource,
        _mime: Option<String>,
    ) {
    }

    fn send_request(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _source: &wayland_client::protocol::wl_data_source::WlDataSource,
        _mime: String,
        _write_pipe: WritePipe,
    ) {
    }

    fn cancelled(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _source: &wayland_client::protocol::wl_data_source::WlDataSource,
    ) {
    }

    fn dnd_dropped(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _source: &wayland_client::protocol::wl_data_source::WlDataSource,
    ) {
    }

    fn dnd_finished(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _source: &wayland_client::protocol::wl_data_source::WlDataSource,
    ) {
    }

    fn action(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _source: &wayland_client::protocol::wl_data_source::WlDataSource,
        _action: wayland_client::protocol::wl_data_device_manager::DndAction,
    ) {
    }
}

impl DataOfferHandler for PinEntryWindow {
    fn source_actions(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _offer: &mut DragOffer,
        _actions: wayland_client::protocol::wl_data_device_manager::DndAction,
    ) {
    }

    fn selected_action(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _offer: &mut DragOffer,
        _actions: wayland_client::protocol::wl_data_device_manager::DndAction,
    ) {
    }
}

delegate_compositor!(PinEntryWindow);
delegate_output!(PinEntryWindow);
delegate_shm!(PinEntryWindow);
delegate_seat!(PinEntryWindow);
delegate_keyboard!(PinEntryWindow);
delegate_pointer!(PinEntryWindow);
delegate_xdg_shell!(PinEntryWindow);
delegate_xdg_window!(PinEntryWindow);
delegate_data_device!(PinEntryWindow);
delegate_registry!(PinEntryWindow);

impl ProvidesRegistryState for PinEntryWindow {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

fn keysym_to_char(keysym: Keysym) -> Option<char> {
    let key = keysym.raw();

    if (0x20..=0x7e).contains(&key) {
        return Some(key as u8 as char);
    }

    None
}
