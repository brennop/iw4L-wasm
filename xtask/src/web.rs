//! `web [--profile NAME] [--no-opt]`: build the browser entry into `dist/web/`.
//!
//! cargo build (wasm32, `launcher`'s `iw4l` bin, `[profile.web]` by default) ->
//! wasm-bindgen -> optional wasm-opt -> `iw4l_bg.wasm.gz` -> `index.html` next to
//! the glue, its URLs tagged with the wasm's hash. The `wasm-bindgen` CLI has to
//! match the crate version in `Cargo.lock` exactly, so that version is read
//! from the lockfile; a matching binary on PATH or under `target/tools/` is
//! used, otherwise it is installed into `target/tools/` (repo-local, ignored
//! by git, never touches `~/.cargo/bin`).

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::shell::{self, Res, Step};

const TARGET: &str = "wasm32-unknown-unknown";
const BIN: &str = "iw4l";

/// Features the rustc wasm32 output uses that binaryen leaves off by default.
const WASM_OPT_FLAGS: [&str; 7] = [
    "-Oz",
    "--enable-bulk-memory",
    "--enable-nontrapping-float-to-int",
    "--enable-sign-ext",
    "--enable-mutable-globals",
    "--enable-reference-types",
    "--enable-multivalue",
];

pub fn run(root: &Path, args: &[String]) -> Res<()> {
    let mut profile = "web".to_owned();
    let mut optimise = true;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--profile" => {
                profile = iter.next().ok_or("--profile needs a value")?.clone();
            }
            "--no-opt" => optimise = false,
            other => {
                return Err(format!(
                    "usage: web [--profile NAME] [--no-opt] (got {other})"
                ));
            }
        }
    }
    let version = lock_version(root, "wasm-bindgen")?;
    let bindgen = wasm_bindgen_cli(root, &version)?;

    let step = Step::start("web build", &format!("profile={profile} target={TARGET}"));
    shell::run(
        Command::new("cargo")
            .current_dir(root)
            .args(["build", "--target", TARGET, "--profile", &profile])
            .args(["-p", "launcher", "--bin", BIN]),
    )?;
    step.done("");
    let dir = if profile == "dev" { "debug" } else { &profile };
    let input = root
        .join("target")
        .join(TARGET)
        .join(dir)
        .join(format!("{BIN}.wasm"));

    let out = root.join("dist/web");
    if out.exists() {
        std::fs::remove_dir_all(&out).map_err(|e| format!("clear {}: {e}", out.display()))?;
    }
    std::fs::create_dir_all(&out).map_err(|e| format!("create {}: {e}", out.display()))?;
    let step = Step::start("web bindgen", &format!("wasm-bindgen {version}"));
    shell::run(
        Command::new(&bindgen)
            .args(["--target", "web", "--no-typescript", "--out-dir"])
            .arg(&out)
            .arg(&input),
    )?;
    step.done("");
    let wasm = out.join(format!("{BIN}_bg.wasm"));
    let raw = file_len(&wasm)?;

    if !optimise {
        println!("web opt: skipped (--no-opt)");
    } else if shell::tool_on_path("wasm-opt") {
        let step = Step::start("web opt", "wasm-opt -Oz");
        let tmp = out.join("opt.wasm");
        shell::run(
            Command::new("wasm-opt")
                .args(WASM_OPT_FLAGS)
                .arg(&wasm)
                .arg("-o")
                .arg(&tmp),
        )?;
        std::fs::rename(&tmp, &wasm).map_err(|e| format!("replace {}: {e}", wasm.display()))?;
        step.done("");
    } else {
        println!("web opt: skipped, wasm-opt (binaryen) not on PATH");
    }

    let page = root.join("crates/launcher/web/index.html");
    let html =
        std::fs::read_to_string(&page).map_err(|e| format!("read {}: {e}", page.display()))?;
    let tag = &crate::release::file_sha256(&wasm)?[..12];
    let html = versioned_page(&html, tag)?;
    std::fs::write(out.join("index.html"), html)
        .map_err(|e| format!("write {}/index.html: {e}", out.display()))?;
    let mixer = root.join("crates/launcher/web/iw4l-mixer.js");
    std::fs::copy(&mixer, out.join("iw4l-mixer.js"))
        .map_err(|e| format!("copy {}: {e}", mixer.display()))?;

    let fin = file_len(&wasm)?;
    let mb = |bytes: u64| bytes as f64 / 1_000_000.0;
    println!("web: {}", out.display());
    println!("web: {BIN}_bg.wasm raw {:.1} MB ({raw} bytes)", mb(raw));
    println!("web: {BIN}_bg.wasm final {:.1} MB ({fin} bytes)", mb(fin));
    match write_gzip(&wasm) {
        Some(gz) => println!(
            "web: {BIN}_bg.wasm.gz gzip -9 {:.1} MB ({gz} bytes)",
            mb(gz)
        ),
        None => println!("web: gzip size unavailable (gzip not on PATH)"),
    }
    println!("web: serve with `make web-serve`");
    Ok(())
}

/// The glue's internal export names change with every build, so a cached
/// `iw4l.js` from one build fails against another build's wasm (a CDN such as a
/// Cloudflare tunnel caches `.js` by default). Both URLs carry the wasm's hash.
fn versioned_page(html: &str, tag: &str) -> Res<String> {
    let glue = format!("./{BIN}.js");
    let import = format!("from '{glue}';");
    let wasm = format!("'./{BIN}_bg.wasm'");
    if !html.contains(&import) || !html.contains(&wasm) {
        return Err(format!(
            "index.html: expected `{import}` and `{wasm}` to version the build"
        ));
    }
    let mixer = "'./iw4l-mixer.js'";
    if !html.contains(mixer) {
        return Err(format!("index.html: expected {mixer} to version the build"));
    }
    Ok(html
        .replace(mixer, &format!("'./iw4l-mixer.js?v={tag}'"))
        .replace(&import, &format!("from '{glue}?v={tag}';"))
        .replace(&wasm, &format!("'./{BIN}_bg.wasm?v={tag}'")))
}

fn file_len(path: &Path) -> Res<u64> {
    std::fs::metadata(path)
        .map(|meta| meta.len())
        .map_err(|e| format!("stat {}: {e}", path.display()))
}

/// Writes `path` + `.gz` (gzip -9, for `scripts/web_serve.py`) and returns its size.
fn write_gzip(path: &Path) -> Option<u64> {
    let output = Command::new("gzip")
        .args(["-9", "-c", "-n"])
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut name = path.as_os_str().to_owned();
    name.push(".gz");
    std::fs::write(name, &output.stdout).ok()?;
    Some(output.stdout.len() as u64)
}

/// The version of the locked package `name` in `Cargo.lock`.
fn lock_version(root: &Path, name: &str) -> Res<String> {
    let lock = std::fs::read_to_string(root.join("Cargo.lock"))
        .map_err(|e| format!("read Cargo.lock: {e}"))?;
    let mut lines = lock.lines();
    let header = format!("name = \"{name}\"");
    while let Some(line) = lines.next() {
        if line == header
            && let Some(version) = lines
                .next()
                .and_then(|l| l.strip_prefix("version = \""))
                .and_then(|l| l.strip_suffix('"'))
        {
            return Ok(version.to_owned());
        }
    }
    Err(format!("{name} is not in Cargo.lock"))
}

fn is_version(binary: &Path, version: &str) -> bool {
    Command::new(binary)
        .arg("--version")
        .output()
        .is_ok_and(|out| {
            String::from_utf8_lossy(&out.stdout).trim() == format!("wasm-bindgen {version}")
        })
}

/// A `wasm-bindgen` binary of exactly `version`: PATH, then `target/tools/`,
/// then `cargo install` into `target/tools/`.
fn wasm_bindgen_cli(root: &Path, version: &str) -> Res<PathBuf> {
    let on_path = PathBuf::from("wasm-bindgen");
    if is_version(&on_path, version) {
        return Ok(on_path);
    }
    let tools = root.join("target/tools");
    let local = tools.join("bin/wasm-bindgen");
    if is_version(&local, version) {
        return Ok(local);
    }
    let step = Step::start(
        "web install",
        &format!("wasm-bindgen-cli {version} into {}", tools.display()),
    );
    shell::run(
        Command::new("cargo")
            .args([
                "install",
                "wasm-bindgen-cli",
                "--version",
                version,
                "--locked",
            ])
            .arg("--root")
            .arg(&tools),
    )?;
    step.done("");
    if is_version(&local, version) {
        Ok(local)
    } else {
        Err(format!("installed wasm-bindgen is not {version}"))
    }
}
