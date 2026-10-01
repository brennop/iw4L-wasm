// `online` = the `online` feature. Native: quinn + tokio on a worker thread.
// wasm32: the join-only WebTransport backend (`master/conn_web.rs`,
// `master/rt_web.rs`) on the page's event loop. The online dependencies are
// target-specific, so code that needs one side only also checks
// `target_arch`.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(online)");
    println!("cargo::rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_FEATURE_ONLINE").is_some() {
        println!("cargo::rustc-cfg=online");
    }
}
