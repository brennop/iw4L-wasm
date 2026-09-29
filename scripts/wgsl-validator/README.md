# WGSL validator (H3)

Runs the dumped SM3 WGSL through the browser's compiler (`createShaderModule` + `getCompilationInfo`).

1. Dump: `IW4L_SM3_FIXED_SLOTS=1 IW4L_WGSL_DUMP= iw4l --render-acceptance <dir> map <zone>` (writes `iw4l-artifacts/wgsl/`).
2. `scripts/wgsl-validator/serve.sh` (serves this page plus `iw4l-artifacts/wgsl/` and a generated manifest on :8765).
3. Open `http://localhost:8765/` in Chrome; the page prints errors grouped by message (`window.wgslResult`).
