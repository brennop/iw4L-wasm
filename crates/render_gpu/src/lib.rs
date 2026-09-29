pub mod diag;
pub mod drawsurf;
mod plugin;

pub use drawsurf::*;
pub use plugin::RenderGpuPlugin;

/// Whether the renderer targets what a browser's WebGPU offers: default limits, BC textures
/// only, no bindless. Always on for wasm32; natively `IW4L_GPU_PROFILE=webgpu`.
pub fn web_profile() -> bool {
    static WEB: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *WEB.get_or_init(|| {
        cfg!(target_arch = "wasm32")
            || std::env::var("IW4L_GPU_PROFILE").is_ok_and(|v| v.eq_ignore_ascii_case("webgpu"))
    })
}
