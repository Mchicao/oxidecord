//! Bakes the JSON theme files in `themes/` into the binary, and on Windows the
//! app icon into the exe's resources.
//!
//! The list is generated rather than written by hand so dropping a new file
//! into `themes/` is all it takes to ship another preset. `include_str!` pulls
//! the contents in at compile time, so the binary carries every preset and runs
//! without the folder beside it.

use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo::rerun-if-changed=themes");

    let themes_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR")).join("themes");

    let mut files: Vec<PathBuf> = fs::read_dir(&themes_dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
                .collect()
        })
        .unwrap_or_default();
    // Read order decides the order presets are parsed in, so keep it stable.
    files.sort();

    let mut generated = String::from(
        "/// Every JSON file in `themes/`, as `(file name, contents)`, in name order.\n",
    );
    generated.push_str("pub static PRESET_THEME_FILES: &[(&str, &str)] = &[\n");
    for path in &files {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        // Forward slashes: `include_str!` takes them on every platform, and a
        // Windows path would otherwise be full of escape sequences.
        let path = path.display().to_string().replace('\\', "/");
        generated.push_str(&format!("    (\"{name}\", include_str!(\"{path}\")),\n"));
    }
    generated.push_str("];\n");

    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("preset_themes.rs");
    fs::write(&out, generated).expect("failed to write generated preset theme list");

    // Checked on the target rather than with `cfg!`, which would describe the
    // host the build script runs on.
    if env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows") {
        println!("cargo::rerun-if-changed=assets/icon.ico");
        embed_resource::compile("assets/icon.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("failed to embed the app icon");
    }
}
