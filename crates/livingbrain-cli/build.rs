//! Embeds the web app's own files into the CLI, so `livingbrain view` serves
//! the same 3D brain front end the site serves: `app/brain.html` and everything
//! under `app/assets/`, read from the source tree (not a copy or a fork). The
//! generated table is `$OUT_DIR/embedded.rs`, included by `src/view.rs`.
//!
//! Std only. Nothing in `app/` is changed by the build.

use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets this"));
    let app = manifest.join("../../app");
    println!("cargo:rerun-if-changed={}", app.display());

    let mut entries: Vec<(String, PathBuf)> = Vec::new();
    let brain = app.join("brain.html");
    entries.push(("brain.html".to_owned(), brain));
    collect(&app.join("assets"), "assets", &mut entries);
    entries.sort();

    let mut out = String::from(
        "/// Every embedded file of the web app: `(path under app/, bytes)`. Generated.\n\
         pub static FILES: &[(&str, &[u8])] = &[\n",
    );
    for (name, path) in &entries {
        println!("cargo:rerun-if-changed={}", path.display());
        out.push_str(&format!(
            "    ({name:?}, include_bytes!({:?}) as &[u8]),\n",
            path.display().to_string()
        ));
    }
    out.push_str("];\n");

    let dest =
        PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets this")).join("embedded.rs");
    fs::write(dest, out).expect("write embedded.rs");
}

/// Every file under `dir`, named `prefix/<relative path>`, with `tests`
/// directories and hidden files left out.
fn collect(dir: &Path, prefix: &str, entries: &mut Vec<(String, PathBuf)>) {
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut children: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .collect();
    children.sort();
    for path in children {
        let name = path
            .file_name()
            .expect("a name")
            .to_string_lossy()
            .into_owned();
        if name.starts_with('.') || name == "tests" {
            continue;
        }
        let key = format!("{prefix}/{name}");
        if path.is_dir() {
            collect(&path, &key, entries);
        } else {
            entries.push((key, path));
        }
    }
}
