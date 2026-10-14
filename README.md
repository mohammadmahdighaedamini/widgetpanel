# Right Panel

A lightweight side-panel app for Windows, written in Rust with [wry](https://github.com/tauri-apps/wry). It docks to the side of your screen and gives you quick access to everyday tools.

## Features

- Emoji picker
- Clipboard history
- Color tools
- Notes
- Quick app launcher

## Requirements

- Windows 10 / 11
- [Rust](https://www.rust-lang.org/tools/install) (stable toolchain)
- WebView2 Runtime (included with recent Windows versions)

## Build & Run

```bash
# Run in debug mode
cargo run

# Build an optimized release binary
cargo build --release
```

The compiled executable will be in `target/release/`.

## Project Structure

```
.
├── .cargo/       # Cargo configuration
├── assets/       # Icons, images and UI resources
├── installer/    # Installer files
├── src/          # Rust source code
├── build.rs      # Build script
├── Cargo.toml    # Project manifest
└── Cargo.lock    # Dependency lock file
```

## License

This project is licensed under the [MIT License](LICENSE).
