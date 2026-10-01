//! Chooses the linker script: `kairos_iram.x` -- linkall.x with the hot code
//! moved to IRAM, see that file -- or with `--features flash-code` esp-hal's
//! own `linkall.x`.

fn main() {
    println!("cargo:rerun-if-changed=kairos_iram.x");
    if std::env::var_os("CARGO_FEATURE_FLASH_CODE").is_none() {
        let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
        println!("cargo:rustc-link-search={dir}");
        println!("cargo:rustc-link-arg=-Tkairos_iram.x");
    } else {
        println!("cargo:rustc-link-arg=-Tlinkall.x");
    }
}
