//! Guards the bundled-icon contract: every symbolic icon name the code uses has an SVG in
//! `assets/icons/`, every bundled SVG parses under GTK's strict vector parser, and the registered
//! theme resolves them.
//!
//! This is its own test binary because GTK may only be initialised on one thread per process,
//! and the window tests in the app already claim that for theirs.

#[path = "../src/icons.rs"]
mod icons;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir)
        .expect("read a source directory")
        .flatten()
    {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The `"…-symbolic"` string literals in `text`.
fn symbolic_literals(text: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for piece in text.split('"').skip(1).step_by(2) {
        let valid = piece
            .strip_suffix("-symbolic")
            .is_some_and(|stem| !stem.is_empty())
            && piece
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
        if valid {
            names.insert(piece.to_string());
        }
    }
    names
}

/// Names of the bundled SVGs (file stems) under `assets/icons`.
fn bundled() -> BTreeSet<String> {
    fn walk(dir: &Path, out: &mut BTreeSet<String>) {
        for entry in fs::read_dir(dir).expect("read assets/icons").flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "svg") {
                out.insert(path.file_stem().unwrap().to_string_lossy().into_owned());
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(&manifest().join("assets/icons"), &mut out);
    out
}

#[test]
fn every_symbolic_name_in_the_sources_is_bundled() {
    let mut files = Vec::new();
    rust_files(&manifest().join("src"), &mut files);
    assert!(!files.is_empty(), "found no sources to scan");

    let have = bundled();
    let mut missing = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file).expect("read a source file");
        for name in symbolic_literals(&text) {
            if !have.contains(&name) {
                missing.push(format!("{name} (in {})", file.display()));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "icons used in code but not under assets/icons/: {missing:#?}"
    );
}

#[test]
fn every_owned_icon_constant_has_a_file() {
    let have = bundled();
    for name in icons::ICONS {
        assert!(
            have.contains(*name),
            "ICONS lists {name}, which is not bundled"
        );
    }
}

#[test]
fn bundled_svgs_are_plain_enough_for_gtks_parser() {
    // GTK 4.22 rejects font/style attributes and ignores <g>; a transform on the path is what
    // makes it render the vector at the requested size instead of upscaling a 16 px raster.
    fn walk(dir: &Path, bad: &mut Vec<String>) {
        for entry in fs::read_dir(dir).expect("read assets/icons").flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, bad);
            } else if path.extension().is_some_and(|e| e == "svg") {
                let text = fs::read_to_string(&path).expect("read an svg");
                // The comment header is prose; judge only the markup after it.
                let body = text.rsplit("-->").next().unwrap_or(&text);
                for banned in ["<g", "style=", "font-", "overflow=", "<defs", "<use"] {
                    if body.contains(banned) {
                        bad.push(format!("{}: contains {banned}", path.display()));
                    }
                }
                if body.contains("<path") && !body.contains("transform=") {
                    bad.push(format!("{}: path without a transform", path.display()));
                }
            }
        }
    }
    let mut bad = Vec::new();
    walk(&manifest().join("assets/icons"), &mut bad);
    assert!(bad.is_empty(), "{bad:#?}");
}

#[test]
fn registered_theme_resolves_bundled_icons() {
    if gtk4::init().is_err() {
        eprintln!("no display; skipping the icon theme check");
        return;
    }
    assert!(icons::register(), "registering the bundled icons failed");
    let display = gtk4::gdk::Display::default().expect("display after init");
    let theme = gtk4::IconTheme::for_display(&display);
    for name in [
        icons::CLAUDE_ICON,
        icons::AGY_ICON,
        icons::CODEX_ICON,
        icons::THINKING_ICON,
        icons::APP_ICON,
        "chat-message-new-symbolic",
        "go-up-symbolic",
    ] {
        assert!(theme.has_icon(name), "icon theme cannot resolve {name}");
    }
}
