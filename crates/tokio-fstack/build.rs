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

    // F-Stack keeps its boot hooks in ELF linker-set sections. The default
    // `--gc-sections` drops sections referenced only by weak __start_/__stop_
    // symbols, so keep all sections.
    println!("cargo:rustc-link-arg=-Wl,--no-gc-sections");

    // F-Stack's DPDK glue calls the bonding PMD directly (PMDs are not part
    // of libdpdk-libs) and rte_timer_meta_init, which only exists in the
    // static timer archive.
    println!("cargo:rustc-link-lib=dylib=rte_net_bond");
    println!("cargo:rustc-link-lib=static=rte_timer");
    // PMDs used by vdev-based test setups (net_tap / null). Linking the shared
    // objects also registers them with the EAL driver registry.
    for lib in ["rte_net_tap", "rte_net_null", "rte_net_ring"] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }

    for lib in ["rt", "m", "dl", "crypto", "z", "pthread", "numa"] {
        println!("cargo:rustc-link-lib={lib}");
    }
}
