use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => "x64",
        Ok("x86") => "x86",
        Ok("aarch64") | Ok("arm64ec") => "arm64",
        Ok("arm") => "arm32",
        _ => return,
    };
    // VCToolsInstallDir is set inside a Developer prompt; otherwise skip quietly.
    if let Some(dir) = std::env::var_os("VCToolsInstallDir") {
        let libs = PathBuf::from(dir).join("lib").join("spectre").join(arch);
        if libs.exists() {
            println!("cargo:rustc-link-search=native={}", libs.display());
        }
    }
}
