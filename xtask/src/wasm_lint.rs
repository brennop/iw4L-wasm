//! `cargo xtask wasm-lint`: the number of call sites that panic or are
//! unsupported in the browser, as clippy `disallowed_methods` hits for
//! `wasm32-unknown-unknown`. The rule list is `xtask/wasm-lint/clippy.toml`,
//! kept out of the root `clippy.toml` because natively `web_time` re-exports
//! the std types and the lint would fire everywhere.
//!
//! Prints the count on stdout; the sites go to stderr.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::shell::Res;

pub fn run_cli(root: &Path) -> Res<()> {
    let out = Command::new("cargo")
        .current_dir(root)
        .env("CLIPPY_CONF_DIR", root.join("xtask").join("wasm-lint"))
        .args([
            "clippy",
            "--target",
            "wasm32-unknown-unknown",
            "-p",
            "launcher",
            "--message-format=json",
            "--",
            "-W",
            "clippy::disallowed_methods",
        ])
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("cargo clippy: {e}"))?;
    let mut sites = BTreeSet::new();
    let mut compile_errors = 0;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if msg["reason"] != "compiler-message" {
            continue;
        }
        let diag = &msg["message"];
        if diag["level"] == "error" {
            compile_errors += 1;
        }
        if diag["code"]["code"] != "clippy::disallowed_methods" {
            continue;
        }
        // Build scripts run on the host, whatever the target is.
        let Some(span) = diag["spans"]
            .as_array()
            .and_then(|s| s.iter().find(|s| s["is_primary"] == true))
        else {
            continue;
        };
        let file = span["file_name"].as_str().unwrap_or("?");
        if file.ends_with("build.rs") {
            continue;
        }
        sites.insert(format!(
            "{}:{}:{}: {}",
            file,
            span["line_start"],
            span["column_start"],
            diag["message"].as_str().unwrap_or("")
        ));
    }
    if !out.status.success() || compile_errors > 0 {
        return Err(
            "cargo clippy --target wasm32-unknown-unknown failed; fix the build first".into(),
        );
    }
    for site in &sites {
        eprintln!("{site}");
    }
    println!("{}", sites.len());
    Ok(())
}
