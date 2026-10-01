#![allow(clippy::disallowed_methods, reason = "build scripts are exempt")]
#[cfg(target_os = "linux")]
fn main() {
    use std::{env, path::PathBuf};

    // libghostty-vt is prebuilt (`zig build -Demit-lib-vt` in an upstream
    // Ghostty checkout), kept outside the repo in `<project>/ghostty-vt/` next
    // to the worktrees; see README.md here. GHOSTTY_VT_DIR overrides that.
    println!("cargo:rerun-if-env-changed=GHOSTTY_VT_DIR");
    // `<project>/worktrees/<name>/crates/ghostty_vt_sys`, or a plain clone at
    // `<project>/<repo>/crates/ghostty_vt_sys`.
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let candidates = match env::var_os("GHOSTTY_VT_DIR") {
        Some(dir) => vec![PathBuf::from(dir)],
        None => vec![manifest.join("../../../../ghostty-vt"), manifest.join("../../../ghostty-vt")],
    };
    let kit_dir = candidates
        .iter()
        .find_map(|dir| dir.canonicalize().ok())
        .unwrap_or_else(|| {
            panic!(
                "libghostty-vt not found at {}; see crates/ghostty_vt_sys/README.md or set GHOSTTY_VT_DIR",
                candidates[0].display()
            )
        });
    let header = kit_dir.join("include/ghostty/vt.h");
    let library = kit_dir.join("lib/libghostty-vt.a");
    println!("cargo:rerun-if-changed={}", header.display());
    println!("cargo:rerun-if-changed={}", library.display());
    println!("cargo:rustc-link-search=native={}", kit_dir.join("lib").display());
    println!("cargo:rustc-link-lib=static=ghostty-vt");

    let bindings = bindgen::Builder::default()
        .header(header.to_string_lossy())
        .clang_arg(format!("-I{}", kit_dir.join("include").display()))
        .allowlist_function("ghostty_.*")
        .allowlist_type("Ghostty.*")
        .allowlist_var("GHOSTTY_.*")
        .prepend_enum_name(false)
        .layout_tests(false)
        .generate()
        .expect("unable to generate libghostty-vt bindings");
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("couldn't write libghostty-vt bindings");
}

#[cfg(not(target_os = "linux"))]
fn main() {}
