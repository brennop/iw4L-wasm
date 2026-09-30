# Patched wgpu

Base: wgpu 29.0.4, the published crates.io source (`.cargo_vcs_info.json`,
`.cargo-ok` and `Cargo.lock` removed; `Cargo.toml` unchanged, so wgpu-core, -hal
and -types stay registry deps at 29.0.4).

Patch: `Device::create_render_pipeline_async` and
`Device::create_compute_pipeline_async`, from gfx-rs/wgpu PR #10438, as
backported onto v29.0.3 by maxcelar in
https://github.com/maxcelar/wgpu/commit/b172a3e263dc6e0d47df430f9284df30bf741971
(only the `wgpu/src` hunks; CHANGELOG and tests skipped).

Files changed: `src/api/device.rs`, `src/backend/webgpu.rs`, `src/dispatch.rs`.

Conflicts: none. `src/` is byte-identical between wgpu 29.0.3 and 29.0.4, so the
diff applied cleanly.

Difference from the PR: the backport reads `GPUPipelineError.reason`/`message`
reflectively (no new web_sys bindings). `"validation"` maps to Validation and
anything else, including a non-GPUPipelineError, to Internal. The PR maps
`"internal"` to Internal and any other reason to Validation. The two agree for
every value the spec allows.

Remove this directory and the `[patch.crates-io]` entry once a Bevy release
ships a wgpu with this API.
