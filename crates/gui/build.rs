//! Compile the Slint UI and (on Windows) embed the manifest + icon so the
//! executable runs unelevated and shows the right icon in Explorer.
//!
//! Two modes:
//! * default feature `slint-compile`: ui/app.slint is compiled by
//!   `slint-build` (in-process i-slint-compiler). Needs a machine with a
//!   couple of GB of RAM.
//! * `--no-default-features`: the checked-in `src/slint_generated.rs` is used
//!   as-is. This keeps tiny CI boxes (1 GB RAM, where linking
//!   i-slint-compiler is OOM-killed) able to build and cross-check everything.
//!   Regenerate that file with `packaging/regenerate-slint.sh` (or a normal
//!   featureful build) whenever the .slint sources change.

fn main() {
    println!("cargo:rerun-if-changed=ui");
    println!("cargo:rerun-if-changed=assets");
    println!("cargo:rerun-if-changed=vendor/cino-ui");
    println!("cargo:rerun-if-changed=../../packaging/windows");
    println!("cargo:rerun-if-env-changed=UDM_EMBED_RC");

    #[cfg(feature = "slint-compile")]
    {
        slint_build::compile_with_config(
            "ui/app.slint",
            slint_build::CompilerConfiguration::new()
                .with_library_paths(std::collections::HashMap::from([(
                    "cino-ui".into(),
                    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                        .join("vendor/cino-ui/index-compat.slint"),
                )]))
                .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles)
                .with_style("fluent".into()),
        )
        .expect("the .slint sources must compile");
    }

    #[cfg(not(feature = "slint-compile"))]
    {
        let generated = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("slint_generated.rs");
        assert!(
            generated.exists(),
            "src/slint_generated.rs is missing: build once with --features \
             slint-compile (or run packaging/regenerate-slint.sh)"
        );
        println!("cargo:rerun-if-changed=src/slint_generated.rs");
        println!(
            "cargo:rustc-env=SLINT_INCLUDE_GENERATED={}",
            generated.display()
        );
    }

    // In a build script, `#[cfg(target_os = ...)]` describes the HOST, so it
    // would skip resource embedding when cross-compiling to Windows from
    // Linux/macOS. `CARGO_CFG_TARGET_OS` describes the actual target.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // The Slint-generated AppWindow is a very large struct; initialising
        // it on Windows' default 1-2 MB main-thread stack overflows before the
        // first frame (a silent abort for a windows_subsystem="windows" app).
        // Reserve 8 MB - verified on real Windows 11 hardware.
        match std::env::var("CARGO_CFG_TARGET_ENV").as_deref() {
            Ok("msvc") => println!("cargo:rustc-link-arg=/STACK:8388608"),
            _ => println!("cargo:rustc-link-arg=-Wl,--stack,8388608"),
        }
        embed_windows_resources();
    }
}

fn embed_windows_resources() {
    if std::env::var("UDM_EMBED_RC")
        .map(|v| v == "0")
        .unwrap_or(false)
    {
        println!("cargo:warning=UDM_EMBED_RC=0 - skipping manifest/icon embedding");
        return;
    }
    let rc = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("packaging")
        .join("windows")
        .join("udm.rc");
    if !rc.exists() {
        println!(
            "cargo:warning={} not found - no manifest embedded",
            rc.display()
        );
        return;
    }
    // windres / rc.exe resolve bare resource names (ICON "icon.ico",
    // RT_MANIFEST "app.manifest") against their include dirs and the CWD -
    // which is the crate root here, not the .rc's directory. Hand them both
    // locations explicitly so embedding works on Windows *and* when cross
    // compiling from Linux/macOS.
    let include_dirs = vec![
        rc.parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default(),
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets"),
    ];

    // On Windows, windres or rc.exe might be located in toolchains
    // (w64devkit, msys64, or Windows SDK) not in PATH by default.
    if let Ok(current_path) = std::env::var("PATH") {
        let has_rc = current_path.split(';').any(|p| {
            std::path::Path::new(p).join("rc.exe").exists()
                || std::path::Path::new(p).join("windres.exe").exists()
        });
        if !has_rc {
            let known_dirs = [
                r"C:\Soft\w64devkit\bin",
                r"C:\msys64\mingw64\bin",
                r"C:\msys64\ucrt64\bin",
            ];
            let mut found = false;
            for dir in known_dirs {
                let p = std::path::Path::new(dir);
                if p.join("windres.exe").exists() || p.join("rc.exe").exists() {
                    std::env::set_var("PATH", format!("{};{}", dir, current_path));
                    found = true;
                    break;
                }
            }
            if !found {
                if let Ok(entries) =
                    std::fs::read_dir(r"C:\Program Files (x86)\Windows Kits\10\bin")
                {
                    let mut best_dir = None;
                    for entry in entries.flatten() {
                        let candidate = entry.path().join("x64");
                        if candidate.join("rc.exe").exists() {
                            best_dir = Some(candidate);
                        }
                    }
                    if let Some(dir) = best_dir {
                        std::env::set_var("PATH", format!("{};{}", dir.display(), current_path));
                    }
                }
            }
        }
    }

    let verdict = embed_resource::compile(&rc, embed_resource::ParamsIncludeDirs(include_dirs));
    let embedded = match verdict {
        embed_resource::CompilationResult::Ok => true,
        embed_resource::CompilationResult::NotWindows => false,
        v => {
            println!(
                "cargo:warning=could not embed the Windows manifest ({v}) - \
                 set UDM_EMBED_RC=0 to silence this"
            );
            false
        }
    };
    if embedded {
        // MSVC's link.exe adds a default manifest unless told not to;
        // mingw's ld adds none, and `/MANIFEST:NO` is not valid ld syntax.
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            println!("cargo:rustc-link-arg=/MANIFEST:NO");
        }
    }
}
