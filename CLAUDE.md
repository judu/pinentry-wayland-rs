# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

This is a Wayland-native pinentry implementation written in Rust. It provides a secure PIN entry dialog for GnuPG and other tools using the Assuan protocol. The project implements a custom Wayland window with password input functionality using smithay-client-toolkit.

## Build Commands

- Build: `cargo build`
- Build release: `cargo build --release`
- Run: `cargo run`
- Check code: `cargo check`
- Run with logging: `RUST_LOG=debug cargo run`

## Architecture

### Core Components

**src/main.rs**
- Entry point and main event loop
- Implements `WaylandPinentry` struct that handles the pinentry protocol
- Implements `PinentryCmds` trait from the `pinentry` crate
- Spawns a separate thread for Wayland event handling to avoid blocking the Assuan protocol
- Uses Arc<Mutex<>> for thread-safe result sharing between Wayland and main threads

**src/wayland_window.rs**
- Complete Wayland window implementation using smithay-client-toolkit
- Handles all Wayland protocol interactions (compositor, seat, keyboard, pointer, data device for clipboard)
- Custom software rendering using direct pixel manipulation
- Text rendering using the `swash` crate for font shaping and rasterization
- Keyboard input handling with modifier key support (Ctrl+V for paste, Backspace, Enter, Escape)
- Clipboard integration via Wayland data device protocol

### Vendored Dependencies

The project vendors the `assuan-rs` library in `vendor/assuan-rs/`:
- `assuan` crate: Low-level Assuan protocol implementation
- `pinentry` crate: High-level pinentry server abstraction

These are used via path dependencies in Cargo.toml rather than from crates.io.

### Threading Model

The application uses a two-thread model:
1. Main thread: Handles Assuan protocol communication via stdin/stdout
2. Wayland thread: Manages the event loop and window rendering

Communication between threads happens via `Arc<Mutex<Option<Result<String, String>>>>` to pass the user's input back to the main thread.

### Rendering

The project does custom software rendering:
- Direct pixel buffer manipulation for backgrounds, input boxes, and cursor
- Password masking using asterisk characters (rendered as pixels, not text)
- Font rendering via `swash` library with proper glyph shaping and alpha blending
- Currently hardcoded to use DejaVu Sans font from `/usr/share/fonts/X11/dejavu/DejaVuSans.ttf`

### Wayland Protocol Usage

Uses smithay-client-toolkit for protocol handling:
- xdg-shell for window management
- wl_seat/wl_keyboard for input
- wl_data_device for clipboard access
- wl_shm for shared memory buffers

## Development Notes

- The project uses Rust edition 2024
- Logging is available via `env_logger` - set `RUST_LOG` environment variable
- Window is fixed at 400x200 pixels (WINDOW_WIDTH and WINDOW_HEIGHT constants)
- Color scheme uses Catppuccin-like dark theme (colors defined in wayland_window.rs:236-240)
- Clipboard paste is asynchronous - clipboard content is read in a background thread
