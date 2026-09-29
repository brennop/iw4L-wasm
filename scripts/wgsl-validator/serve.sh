#!/bin/sh
# Serve the validator page and the dumped WGSL on :8765.
set -e
root=$(cd "$(dirname "$0")/../.." && pwd)
stage=$(mktemp -d)
cp "$root/scripts/wgsl-validator/index.html" "$stage/"
ln -s "$root/iw4l-artifacts/wgsl" "$stage/wgsl"
(cd "$root/iw4l-artifacts/wgsl" && python3 -c 'import json,os;print(json.dumps(sorted(f for f in os.listdir(".") if f.endswith(".wgsl"))))') > "$stage/manifest.json"
cd "$stage" && exec python3 -m http.server 8765
