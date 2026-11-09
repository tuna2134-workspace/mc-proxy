//! F-Stack keeps its boot hooks (SYSINIT/VNET/PCPU sets) in ELF linker-set
//! sections. rustc's default `--gc-sections` drops those sections because the
//! `__start_`/`__stop_` symbols are weak, which leaves `vnet0` uninitialised
//! and crashes `ff_freebsd_init`. Keep all sections for the F-Stack build.
fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_FSTACK");
    if std::env::var_os("CARGO_FEATURE_FSTACK").is_some() {
        println!("cargo:rustc-link-arg=-Wl,--no-gc-sections");
    }
}
