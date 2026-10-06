//! Compiles `assets/icons/` into a GResource that `src/icons.rs` registers at startup.
//!
//! The resource manifest is generated from the directory contents rather than hand-listed, so
//! dropping an SVG under `assets/icons/scalable/<context>/` is all it takes to bundle it.
//! Needs `glib-compile-resources` (Debian: `libglib2.0-dev-bin`).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::{env, fs};

const PREFIX: &str = "/com/jdesroches/AgentTerminal/icons";

fn main() {
    let root = Path::new("assets/icons");
    println!("cargo:rerun-if-changed=assets/icons");
    println!("cargo:rerun-if-changed=assets/com.jdesroches.AgentTerminal.svg");
    println!("cargo:rerun-if-changed=build.rs");

    let mut files = Vec::new();
    collect(root, root, &mut files);
    files.sort();

    let mut xml = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<gresources>\n  <gresource prefix=\"{PREFIX}\">\n");
    for rel in &files {
        let _ = writeln!(xml, "    <file>{rel}</file>");
    }
    xml.push_str("  </gresource>\n");
    // The full-colour app icon, for hero use (`APP_ART` in src/icons.rs).
    xml.push_str("  <gresource prefix=\"/com/jdesroches/AgentTerminal/art\">\n");
    xml.push_str(
        "    <file>com.jdesroches.AgentTerminal.svg</file>\n  </gresource>\n</gresources>\n",
    );

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    let manifest = out_dir.join("icons.gresource.xml");
    fs::write(&manifest, xml).expect("write the icon resource manifest");

    glib_build_tools::compile_resources(
        &["assets/icons", "assets"],
        manifest.to_str().expect("OUT_DIR is valid UTF-8"),
        "icons.gresource",
    );
}

/// Relative paths (forward slashes) of every `.svg` under `dir`.
fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let entries = fs::read_dir(dir).expect("read assets/icons");
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out);
        } else if path.extension().is_some_and(|e| e == "svg") {
            let rel = path.strip_prefix(root).expect("under the icon root");
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}
