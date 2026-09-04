use std::path::PathBuf;
use std::process::Command;

/// Compile the bundled icons into a GResource blob that `src/frontend/gtk_overlay.rs`
/// embeds and registers at startup. Shelling out to `glib-compile-resources`
/// keeps this off the dependency list, so the vendored `cargo-sources.json`
/// used by the Flatpak build stays valid.
fn main() {
    const MANIFEST: &str = "data/cursor-clip.gresource.xml";
    const SOURCE_DIR: &str = "data/icons";

    println!("cargo::rerun-if-changed={MANIFEST}");
    println!("cargo::rerun-if-changed=data/icons");

    let out_dir = PathBuf::from(
        std::env::var_os("OUT_DIR").expect("OUT_DIR is always set for build scripts"),
    );

    let status = Command::new("glib-compile-resources")
        .arg("--sourcedir")
        .arg(SOURCE_DIR)
        .arg("--target")
        .arg(out_dir.join("cursor-clip.gresource"))
        .arg(MANIFEST)
        .status()
        .expect("could not run glib-compile-resources - install the glib development tools");

    assert!(status.success(), "glib-compile-resources failed: {status}");
}
