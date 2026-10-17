# Patched wgpu

Base: wgpu 30.0.1, the published crates.io source (`.cargo_vcs_info.json`,
`.cargo-ok` and `Cargo.lock` removed; `Cargo.toml` unchanged, so wgpu-core, -hal
and -types stay registry deps at 30.0.1).

Patch: `Device::create_render_pipeline_async` and
`Device::create_compute_pipeline_async`, from gfx-rs/wgpu PR #10438, as
backported onto v29.0.3 by maxcelar in
https://github.com/maxcelar/wgpu/commit/b172a3e263dc6e0d47df430f9284df30bf741971
(only the `wgpu/src` hunks; CHANGELOG and tests skipped), then carried from
29.0.4 to 30.0.1.

Files changed: `src/api/device.rs`, `src/backend/webgpu.rs`, `src/dispatch.rs`,
plus `#[allow]`s for wasm32 build warnings in `src/api/buffer.rs` and
`src/macros/mod.rs`.

Carrying it to 30.0.1: `map_render_pipeline_descriptor` and
`map_compute_pipeline_descriptor` are lifted from 30.0.1's own
`create_render_pipeline`/`create_compute_pipeline` (typed `JsNullable` arrays and
the `new_with_gpu_auto_layout_mode` constructors), not from the 29.x bodies. The
web-sys promises are typed in 30, so the completion functions take
`Result<GpuRenderPipeline, JsValue>` instead of converting a `JsValue`.

Difference from the PR: the backport reads `GPUPipelineError.reason`/`message`
reflectively (no new web_sys bindings). `"validation"` maps to Validation and
anything else, including a non-GPUPipelineError, to Internal. The PR maps
`"internal"` to Internal and any other reason to Validation. The two agree for
every value the spec allows.

Remove this directory and the `[patch.crates-io]` entry once a Bevy release
ships a wgpu with this API.
