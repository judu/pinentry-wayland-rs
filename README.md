# pinentry-wayland-rs

A Wayland-native pinentry implementation written in Rust.

## About

This is a pinentry program for GnuPG and other tools that use the Assuan protocol to request PINs and passphrases from users. It provides a native Wayland dialog without requiring X11 or GTK/Qt dependencies.

**Note**: This project was vibe-coded for personal use and is provided as-is.
There is no configuration support - settings like window size, colors, and font path are hardcoded.
At the time of the writing, I’m using [Niri](https://github.com/YaLTeR/niri), so the size of the window doesn’t matter.

**Warning**: As I did it quickly and vibe-coded most of it, I did not bother long with clipboard management.
While the GETPIN window is open, it will fetch in a buffer everything that you copy.
Everytime you copy something (more precisely, everytime Wayland notifies us of a new SelectionOffer)
while the pin entry window is open, this program will override its buffer with the content of the clipboard.

## Building

### Prerequisites

- Rust toolchain (edition 2024)
- Wayland development libraries
- DejaVu Sans font installed at `/usr/share/fonts/X11/dejavu/DejaVuSans.ttf`
  (`grep -r DejaVu src/` to find and update it if needed)

### Build Commands

```bash
# Debug build
cargo build

# Release build
cargo build --release

# Run directly
cargo run

# Run with debug logging
RUST_LOG=debug cargo run
```

The compiled binary will be in `target/release/pinentry-wayland-rs` (or `target/debug/` for debug builds).

## Dependencies

This project uses:

- **smithay-client-toolkit**: Wayland protocol handling and window management
- **swash**: Font shaping and text rendering
- **assuan** (vendored): Low-level Assuan protocol implementation
- **pinentry** (vendored): High-level pinentry server abstraction

The Assuan and pinentry crates are vendored in `vendor/assuan-rs/` and referenced as path dependencies.
I found it at https://github.com/survived/assuan-rs/ but not in crates.rs.

## Usage

Configure your GnuPG agent to use this pinentry by adding to `~/.gnupg/gpg-agent.conf`:

```
pinentry-program /path/to/pinentry-wayland-rs
```

Then reload the agent:

```bash
gpg-connect-agent reloadagent /bye
```

## Features

- Native Wayland support (no X11 required)
- Password masking with asterisks
- Keyboard input with Ctrl+V clipboard paste support
- Custom software rendering
- Assuan protocol compliant

## Limitations

- No configuration file support
- Hardcoded font path (DejaVu Sans only)
- Fixed window size (400x200)
- Hardcoded color scheme
- Basic error handling
- Personal use project - minimal testing

## License

This project is provided as-is for personal use, under [The Unlicense](https://unlicense.org/).
