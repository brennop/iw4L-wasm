#!/usr/bin/env python3
"""Build the browser bundle, link the pack into dist/web and serve it.

usage: web_run.py [--pack FILE] [--port N] [--opt] [--no-build]

  --pack FILE   pack to link as dist/web/game.pack (default: $IW4L_WEB_PACK,
                which may be set in the repo's .env; the real environment wins)
  --port N      port for web_serve.py (default 8080)
  --opt         run wasm-opt (xtask web default); without it the build passes --no-opt
  --no-build    skip `cargo xtask web`, only re-link the pack and serve

`cargo xtask web` clears dist/web, so the pack link is recreated every run.
Stop any running web_serve.py before rebuilding: a server holding dist/web open
makes the build fail (os error 32 on Windows).
"""
import argparse
import os
import shutil
import socket
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DIST = ROOT / "dist" / "web"


def load_dotenv(path):
    """KEY=value lines from .env; the real environment wins."""
    try:
        lines = path.read_text(encoding="utf-8-sig").splitlines()
    except OSError:
        return
    for line in lines:
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        if line.startswith("export "):
            line = line[len("export "):]
        key, _, value = line.partition("=")
        value = value.strip()
        if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
            value = value[1:-1]
        os.environ.setdefault(key.strip(), value)


def port_busy(port):
    with socket.socket() as s:
        return s.connect_ex(("127.0.0.1", port)) == 0


def link_pack(pack):
    target = DIST / "game.pack"
    if target.exists() or target.is_symlink():
        target.unlink()
    try:
        os.symlink(pack, target)
        return "symlink"
    except OSError:
        pass
    try:
        os.link(pack, target)  # same volume only
        return "hardlink"
    except OSError:
        pass
    sys.exit(
        f"cannot link {pack} into {DIST}: symlinks need privileges and hardlinks need the "
        "same volume. Put the pack on the same drive as the repo, enable Developer Mode, "
        "or serve with ?pack=<url>."
    )


def main():
    load_dotenv(ROOT / ".env")
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("--pack", default=os.environ.get("IW4L_WEB_PACK"))
    ap.add_argument("--port", type=int, default=8080)
    ap.add_argument("--opt", action="store_true")
    ap.add_argument("--no-build", action="store_true")
    args = ap.parse_args()

    if port_busy(args.port):
        sys.exit(f"port {args.port} is in use; stop the running server first (it may hold dist/web open)")

    if not args.no_build:
        cmd = ["cargo", "xtask", "web"] + ([] if args.opt else ["--no-opt"])
        print("+", " ".join(cmd), flush=True)
        if subprocess.call(cmd, cwd=ROOT) != 0:
            sys.exit("build failed")
    if not (DIST / "index.html").is_file():
        sys.exit(f"{DIST} is empty; run without --no-build")

    if args.pack:
        pack = Path(args.pack).resolve()
        if not pack.is_file():
            sys.exit(f"pack not found: {pack}")
        print(f"game.pack -> {pack} ({link_pack(pack)})")
    else:
        print("no --pack / IW4L_WEB_PACK: game.pack not linked; open the page with ?pack=<url>")

    print(f"http://127.0.0.1:{args.port}/?map=mp_rust", flush=True)
    py = shutil.which("python3") or sys.executable
    os.chdir(DIST)
    sys.exit(subprocess.call([py, str(ROOT / "scripts" / "web_serve.py"), str(args.port)]))


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        pass
