//! Link-line helper for the F-Stack backend.
//!
//! F-Stack applications link `libfstack.a` with `--whole-archive` plus the DPDK
//! static libraries. Because those flags are installation-specific, they are
//! only emitted when the `fstack` feature is enabled:
//!
//! * `FSTACK_LIB_DIR` — directory containing `libfstack.a` (default
//!   `/usr/local/lib`).
//! * `FSTACK_LIBS` — space-separated library names to link instead of the
//!   defaults (e.g. `fstack dpdk`).
//!
//! When `pkg-config` can find `libdpdk`, its static link flags are added
//! automatically. `cargo check --features fstack` does not link, so it works
//! without an F-Stack installation.

fn main() {
    println!("cargo:rerun-if-env-changed=FSTACK_LIB_DIR");
    println!("cargo:rerun-if-env-changed=FSTACK_LIBS");

    if std::env::var_os("CARGO_FEATURE_FSTACK").is_none() {
        return;
    }

    let lib_dir = std::env::var("FSTACK_LIB_DIR").unwrap_or_else(|_| "/usr/local/lib".to_owned());
    println!("cargo:rustc-link-search=native={lib_dir}");

    // DPDK static libraries first: libfstack depends on them.
    if let Ok(dpdk) = pkg_config::Config::new().statik(true).probe("libdpdk") {
        for path in dpdk.link_paths {
            println!("cargo:rustc-link-search=native={}", path.display());
        }
    } else {
        println!(
            "cargo:warning=pkg-config could not find libdpdk; set PKG_CONFIG_PATH or FSTACK_LIBS"
        );
    }

    match std::env::var("FSTACK_LIBS") {
        Ok(libs) => {
            for lib in libs.split_whitespace() {
                println!("cargo:rustc-link-lib={lib}");
            }
        }
        Err(_) => {
            // F-Stack requires its archive to be linked whole so that the
            // ff_* entry points and constructor tables are pulled in.
            println!("cargo:rustc-link-lib=static:+whole-archive=fstack");
        }
    }

    for lib in ["rt", "m", "dl", "crypto", "z", "pthread", "numa"] {
        println!("cargo:rustc-link-lib={lib}");
    }
}
