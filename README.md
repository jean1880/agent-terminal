# Gemini Terminal 🚀

A robust, standalone GTK4 terminal application written in Rust, specifically designed for a seamless Gemini AI interaction experience.

## Features ✨

- **Native GTK4 & VTE4**: High-performance, GPU-accelerated terminal rendering.
- **Embedded Gemini Theme**: Custom color palette (`#181425` background) baked into the binary.
- **Modern UX**: Sleek HeaderBar design with internal terminal padding for better readability.
- **Interactive Shortcuts**:
  - `Ctrl + Shift + C` / `V`: Copy and Paste.
  - `Ctrl + Plus` / `Minus`: Dynamic text zooming.
  - `Ctrl + 0`: Reset zoom level.
- **Standalone Identity**: Treated as a unique application by your window manager (won't group with standard terminals).
- **Sixel Support**: High-quality image rendering support.

## Prerequisites 🛠️

To build and run Gemini Terminal, you need the following system libraries:

- **Rust & Cargo** (1.70+)
- **GTK 4 Development Files** (`libgtk-4-dev`)
- **VTE 2.91 GTK4 Development Files** (`libvte-2.91-gtk4-dev`)

## Quick Start ⚡

### 1. Install Dependencies
```bash
make deps
```

### 2. Build and Install Locally
This will build the release binary and install it to `~/.local/bin`, while updating your desktop menu entry.
```bash
make install
```

## Advanced Usage 🔧

### Generating a Debian Package (.deb)
If you want to distribute the application or install it system-wide using `dpkg`:
```bash
make package
```
*Note: This requires `cargo-deb`. The Makefile will attempt to install it for you.*

### Development Workflow
- **Build only**: `cargo build`
- **Run for testing**: `cargo run`
- **Clean build artifacts**: `make clean`

## Project Structure 📁

- `src/main.rs`: Core application logic, GTK4 UI definition, and keyboard handling.
- `Cargo.toml`: Rust dependencies and `.deb` packaging metadata.
- `Makefile`: Convenient wrappers for common tasks.
- `LICENSE`: MIT License.

## Contributing 🤝

Contributions are welcome! If you'd like to improve Gemini Terminal:

1. **Fork the repository** (or just work locally in your `Documents` folder).
2. **Create a feature branch**: `git checkout -b feature/cool-new-thing`.
3. **Commit your changes**: `git commit -m "Add something awesome"`.
4. **Adhere to standards**: Ensure your code is formatted with `cargo fmt`.

## License 📄

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
