//! Chooses the linker script: esp-hal's `linkall.x`, or with `--features iram`
//! `kairos_iram.x` -- linkall.x with the hot code moved to IRAM. See that file.

fn main() {
    println!("cargo:rerun-if-changed=kairos_iram.x");
    if std::env::var_os("CARGO_FEATURE_IRAM").is_some() {
        let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
        println!("cargo:rustc-link-search={dir}");
        println!("cargo:rustc-link-arg=-Tkairos_iram.x");
    } else {
        println!("cargo:rustc-link-arg=-Tlinkall.x");
    }
}
