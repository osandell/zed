#![allow(clippy::disallowed_methods, reason = "build scripts are exempt")]
#[cfg(target_os = "macos")]
fn main() {
    use std::{env, path::PathBuf};

    // GhosttyKit is produced by the Ghostty fork's own build
    // (`zig build -Demit-xcframework`), so by default we look for it in the
    // sibling Ghostty checkout. GHOSTTY_KIT_DIR overrides that.
    println!("cargo:rerun-if-env-changed=GHOSTTY_KIT_DIR");
    let kit_dir = match env::var_os("GHOSTTY_KIT_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join(
            "../../../../../ghostty/worktrees/main/macos/GhosttyKit.xcframework/macos-arm64_x86_64",
        ),
    };
    let kit_dir = kit_dir
        .canonicalize()
        .unwrap_or_else(|_| panic!("GhosttyKit not found at {}; build it with `zig build -Demit-xcframework` in the Ghostty checkout or set GHOSTTY_KIT_DIR", kit_dir.display()));

    let header = kit_dir.join("Headers/ghostty.h");
    let library = kit_dir.join("ghostty-internal.a");
    println!("cargo:rerun-if-changed={}", header.display());
    println!("cargo:rerun-if-changed={}", library.display());

    // rustc only finds static archives named lib<name>.a, and GhosttyKit ships
    // `ghostty-internal.a`, so expose it under that name from OUT_DIR.
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    let linked_library = out_path.join("libghostty.a");
    std::fs::remove_file(&linked_library).ok();
    std::os::unix::fs::symlink(&library, &linked_library).expect("couldn't link libghostty.a");
    println!("cargo:rustc-link-search=native={}", out_path.display());
    println!("cargo:rustc-link-lib=static=ghostty");
    println!("cargo:rustc-link-lib=c++");
    for framework in [
        "AppKit",
        "Carbon",
        "CoreFoundation",
        "CoreGraphics",
        "CoreText",
        "CoreVideo",
        "Foundation",
        "IOSurface",
        "Metal",
        "QuartzCore",
    ] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }

    let bindings = bindgen::Builder::default()
        .header(header.to_string_lossy())
        .allowlist_function("ghostty_.*")
        .allowlist_type("ghostty_.*")
        .allowlist_var("GHOSTTY_.*")
        .prepend_enum_name(false)
        .layout_tests(false)
        .generate()
        .expect("unable to generate ghostty bindings");

    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("couldn't write ghostty bindings");
}

#[cfg(not(target_os = "macos"))]
fn main() {}
