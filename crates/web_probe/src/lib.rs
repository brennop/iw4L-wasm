//! Loads one map through `load_prepared_match` and reports what it built.
//! Natively this is the reference; in a browser it is gate G4.

use std::path::PathBuf;

#[cfg(target_arch = "wasm32")]
mod web;

/// Drops `12.3ms`-style timings so two runs of the same load compare equal.
fn strip_timings(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let mut j = i;
            let after_ms_key = out.ends_with("_ms=");
            while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b'.') {
                j += 1;
            }
            if after_ms_key {
                out.push('#');
                i = j;
                continue;
            }
            if line[j..].starts_with("ms") {
                out.push('#');
                i = j + 2;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn fnv1a(text: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

pub struct Report {
    pub lines: Vec<String>,
    pub fingerprint: u64,
    pub load_ms: f64,
}

impl Report {
    pub fn render(&self) -> String {
        let mut text = self.lines.join("\n");
        text.push_str(&format!(
            "\nfingerprint={:016x} lines={} load_ms={:.0}",
            self.fingerprint,
            self.lines.len(),
            self.load_ms
        ));
        text
    }
}

pub async fn probe(games_root: &str, zone: &str) -> Result<Report, String> {
    assets::set_games_root_override(PathBuf::from(games_root));
    let root = assets::GamesRoot(PathBuf::from(games_root));
    let found = assets::find_zone_file(&root, zone)?;
    let common_mp = assets::find_runtime_common_mp(&root, &found.path).map(|zone| zone.path);
    let started = web_time::Instant::now();
    let outcome =
        assets::load_prepared_match(Ok(found.path), common_mp, assets::LoadProgress::default())
            .await;
    let load_ms = started.elapsed().as_secs_f64() * 1000.0;
    let live_at_ready = diag::process_live_heap_bytes();
    let assets::MatchLoadOutcome::Ready(prepared) = outcome else {
        return Err("map walk canceled".to_owned());
    };
    // Stage lines finish in a parallel, run-dependent order and carry only timings.
    let mut lines = prepared
        .report
        .iter()
        .filter(|line| {
            !line.starts_with("load stage")
                && !line.starts_with("resident map")
                && !line.starts_with("IWD read cost")
        })
        .map(|line| strip_timings(line))
        .collect::<Vec<_>>();
    lines.push(format!(
        "counts: static_model_meshes={} static_model_instances={} sound={}",
        prepared.world.static_model_meshes.len(),
        prepared.world.static_model_instances.len(),
        match &prepared.sound {
            Some(Ok(_)) => "ok",
            Some(Err(_)) => "error",
            None => "none",
        }
    ));
    let fingerprint = fnv1a(&lines.join("\n"));
    if let Some(live) = live_at_ready {
        eprintln!("live heap when ready: {} MiB", live >> 20);
    }
    if std::env::var_os("PROBE_BREAKDOWN").is_some() {
        let mut last = diag::process_live_heap_bytes().unwrap_or(0);
        drop(prepared.world);
        let now = diag::process_live_heap_bytes().unwrap_or(0);
        eprintln!("drop world: freed {} MiB", last.saturating_sub(now) >> 20);
        last = now;
        drop(prepared.fx);
        let now = diag::process_live_heap_bytes().unwrap_or(0);
        eprintln!("drop fx: freed {} MiB", last.saturating_sub(now) >> 20);
        last = now;
        drop(prepared.materials);
        let now = diag::process_live_heap_bytes().unwrap_or(0);
        eprintln!(
            "drop materials: freed {} MiB",
            last.saturating_sub(now) >> 20
        );
        last = now;
        drop(prepared.clip);
        let now = diag::process_live_heap_bytes().unwrap_or(0);
        eprintln!("drop clip: freed {} MiB", last.saturating_sub(now) >> 20);
        last = now;
        drop(prepared.weapons);
        let now = diag::process_live_heap_bytes().unwrap_or(0);
        eprintln!("drop weapons: freed {} MiB", last.saturating_sub(now) >> 20);
        last = now;
        drop(prepared.fpv_meshes);
        let now = diag::process_live_heap_bytes().unwrap_or(0);
        eprintln!(
            "drop fpv_meshes: freed {} MiB",
            last.saturating_sub(now) >> 20
        );
        last = now;
        drop(prepared.bodies);
        let now = diag::process_live_heap_bytes().unwrap_or(0);
        eprintln!("drop bodies: freed {} MiB", last.saturating_sub(now) >> 20);
        last = now;
        drop(prepared.world_weapons);
        let now = diag::process_live_heap_bytes().unwrap_or(0);
        eprintln!(
            "drop world_weapons: freed {} MiB",
            last.saturating_sub(now) >> 20
        );
        last = now;
        drop(prepared.projectile_meshes);
        let now = diag::process_live_heap_bytes().unwrap_or(0);
        eprintln!(
            "drop projectile_meshes: freed {} MiB",
            last.saturating_sub(now) >> 20
        );
        last = now;
        drop(prepared.xanims);
        let now = diag::process_live_heap_bytes().unwrap_or(0);
        eprintln!("drop xanims: freed {} MiB", last.saturating_sub(now) >> 20);
        last = now;
        drop(prepared.destructible_death);
        let now = diag::process_live_heap_bytes().unwrap_or(0);
        eprintln!(
            "drop destructible_death: freed {} MiB",
            last.saturating_sub(now) >> 20
        );
        last = now;
        drop(prepared.player_anim_sources);
        let now = diag::process_live_heap_bytes().unwrap_or(0);
        eprintln!(
            "drop player_anim_sources: freed {} MiB",
            last.saturating_sub(now) >> 20
        );
        last = now;
        drop(prepared.tracers);
        let now = diag::process_live_heap_bytes().unwrap_or(0);
        eprintln!("drop tracers: freed {} MiB", last.saturating_sub(now) >> 20);
        last = now;
        drop(prepared.strings);
        let now = diag::process_live_heap_bytes().unwrap_or(0);
        eprintln!("drop strings: freed {} MiB", last.saturating_sub(now) >> 20);
        last = now;
        drop(prepared.prepared_map);
        let now = diag::process_live_heap_bytes().unwrap_or(0);
        eprintln!(
            "drop prepared_map: freed {} MiB",
            last.saturating_sub(now) >> 20
        );
        last = now;
        drop(prepared.sound);
        let now = diag::process_live_heap_bytes().unwrap_or(0);
        eprintln!("drop sound: freed {} MiB", last.saturating_sub(now) >> 20);
        last = now;
        let _ = last;
    } else {
        drop(prepared);
    }
    assets::release_common();
    if let Some(live) = diag::process_live_heap_bytes() {
        eprintln!(
            "live heap after releasing the common set too: {} MiB",
            live >> 20
        );
    }
    if let Some(live) = diag::process_live_heap_bytes() {
        eprintln!("live heap after dropping the match: {} MiB", live >> 20);
    }
    Ok(Report {
        lines,
        fingerprint,
        load_ms,
    })
}
