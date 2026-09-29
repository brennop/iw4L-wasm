// `online` = the `online` feature on a target that has sockets and threads.
// The online dependencies are target-specific, so the feature alone is not
// enough to say they are present.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(online)");
    println!("cargo::rerun-if-changed=build.rs");
    let feature = std::env::var_os("CARGO_FEATURE_ONLINE").is_some();
    let wasm = std::env::var("CARGO_CFG_TARGET_ARCH").is_ok_and(|arch| arch == "wasm32");
    if feature && !wasm {
        println!("cargo::rustc-cfg=online");
    }
}
