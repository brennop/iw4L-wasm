//! Fork-owned memory census of the process (`IW4L_MEM_CENSUS=1`).
//!
//! With the variable unset `MemCensusPlugin` adds nothing: no system, no
//! allocation. With it set, one exclusive system appends a JSON object per
//! snapshot to `mem-census.jsonl` (in `IW4L_MEM_CENSUS_DIR`, else the artifacts
//! dir): after startup, once the hosted room is `in_match`, every
//! `IW4L_MEM_CENSUS_EVERY_S` seconds (default 60), and whenever the file
//! `mem-census.now` appears next to the output (a "final" snapshot; the file is
//! deleted). `cargo xtask mem-census` drives this and reads the result.
//!
//! Byte figures are `capacity * size_of` of the inner Vecs, marked exact (the
//! allocation itself) or approx (a model of a type we cannot walk).

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use bevy::asset::RenderAssetUsages;
use bevy::prelude::*;
use serde_json::{Value, json};

mod probes;
mod process;

pub fn enabled() -> bool {
    std::env::var("IW4L_MEM_CENSUS").is_ok_and(|v| !v.is_empty() && v != "0")
}

pub struct MemCensusPlugin;

impl Plugin for MemCensusPlugin {
    fn build(&self, app: &mut App) {
        if !enabled() {
            return;
        }
        let dir = std::env::var_os("IW4L_MEM_CENSUS_DIR")
            .map(PathBuf::from)
            .or_else(|| asset_transport::ensure_artifacts_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let _ = std::fs::create_dir_all(&dir);
        let every = std::env::var("IW4L_MEM_CENSUS_EVERY_S")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| *v > 0.0)
            .unwrap_or(60.0);
        let path = dir.join("mem-census.jsonl");
        diag::info!(Launch, "mem census: writing {}", path.display());
        if !diag::counting_enabled() {
            diag::info!(
                Launch,
                "mem census: live heap is off; set IW4L_COUNTING_ALLOC=1 (the launcher installs diag::ProcessCountingAllocator)"
            );
        }
        app.insert_resource(CensusState {
            path,
            trigger: dir.join("mem-census.now"),
            started: Instant::now(),
            every: Duration::from_secs_f64(every),
            last: None,
            startup_done: false,
            in_match_done: false,
            last_trigger_check: Instant::now(),
            peak_live_heap: 0,
        })
        .add_systems(Last, census_system);
    }
}

#[derive(Resource)]
struct CensusState {
    path: PathBuf,
    trigger: PathBuf,
    started: Instant,
    every: Duration,
    last: Option<Instant>,
    startup_done: bool,
    in_match_done: bool,
    last_trigger_check: Instant,
    peak_live_heap: u64,
}

fn census_system(world: &mut World) {
    let now = Instant::now();
    let mut label = None;
    // "In match" for the census: the room is hosting in_match and the world spawn
    // (including the handing over of its images) has finished.
    let in_match = world
        .get_resource::<net::MasterBridge>()
        .is_some_and(|b| b.state().in_match())
        && world
            .get_resource::<render_frontend::prepare::scene::world::WorldScene>()
            .is_some_and(|scene| scene.spawned);
    {
        let mut st = world.resource_mut::<CensusState>();
        if !st.startup_done {
            st.startup_done = true;
            label = Some("startup");
        } else if in_match && !st.in_match_done {
            st.in_match_done = true;
            label = Some("in_match");
        } else if st.last.is_some_and(|t| now - t >= st.every) {
            label = Some("periodic");
        }
        if now - st.last_trigger_check >= Duration::from_millis(500) {
            st.last_trigger_check = now;
            if st.trigger.exists() {
                let _ = std::fs::remove_file(&st.trigger);
                label = Some("final");
            }
        }
    }
    let Some(label) = label else {
        return;
    };
    let record = snapshot(world, label);
    let path = {
        let mut st = world.resource_mut::<CensusState>();
        st.last = Some(now);
        st.path.clone()
    };
    append(&path, &record);
    // The destructive, exact pass: only at the end, only when asked for.
    if label == "final"
        && std::env::var("IW4L_MEM_CENSUS_DROP").is_ok_and(|v| !v.is_empty() && v != "0")
    {
        let reverse = std::env::var("IW4L_MEM_CENSUS_DROP_ORDER").is_ok_and(|v| v == "reverse");
        let mut drop = probes::drop_probe(world, reverse);
        drop["label"] = json!("drop");
        drop["process_after"] = process::sample();
        append(&path, &drop);
        world.write_message(AppExit::Success);
    }
}

fn append(path: &std::path::Path, record: &Value) {
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{record}");
    }
}

/// One category row.
pub(crate) struct Cat {
    pub name: String,
    pub count: u64,
    pub bytes: u64,
    pub exact: bool,
}

#[derive(Default)]
pub(crate) struct Cats(pub Vec<Cat>);

impl Cats {
    pub fn add(&mut self, name: impl Into<String>, count: usize, bytes: usize, exact: bool) {
        self.0.push(Cat {
            name: name.into(),
            count: count as u64,
            bytes: bytes as u64,
            exact,
        });
    }
}

fn usage_name(usage: RenderAssetUsages) -> &'static str {
    match (
        usage.contains(RenderAssetUsages::MAIN_WORLD),
        usage.contains(RenderAssetUsages::RENDER_WORLD),
    ) {
        (true, true) => "MAIN|RENDER",
        (true, false) => "MAIN-only",
        (false, true) => "RENDER-only",
        (false, false) => "none",
    }
}

fn snapshot(world: &mut World, label: &str) -> Value {
    let (state_started, peak) = {
        let st = world.resource::<CensusState>();
        (st.started, st.peak_live_heap)
    };
    let proc = process::sample();
    let live = diag::process_live_heap_bytes();
    if let Some(l) = live
        && l > peak
    {
        world.resource_mut::<CensusState>().peak_live_heap = l;
    }
    let mut cats = Cats::default();
    // Assets<Image> / Assets<Mesh> split by usage.
    if let Some(images) = world.get_resource::<Assets<Image>>() {
        let mut by: HashMap<&'static str, (usize, usize)> = HashMap::new();
        for (_, image) in images.iter() {
            let e = by.entry(usage_name(image.asset_usage)).or_default();
            e.0 += 1;
            e.1 += image.data.as_ref().map_or(0, Vec::capacity);
        }
        for (k, (n, b)) in by {
            cats.add(format!("Assets<Image> {k}"), n, b, true);
        }
    }
    if let Some(meshes) = world.get_resource::<Assets<Mesh>>() {
        let mut by: HashMap<&'static str, (usize, usize)> = HashMap::new();
        for (_, mesh) in meshes.iter() {
            let e = by.entry(usage_name(mesh.asset_usage)).or_default();
            e.0 += 1;
            if let Ok(attrs) = mesh.try_attributes() {
                for (_, values) in attrs {
                    e.1 += values.get_bytes().len();
                }
            }
            if let Ok(Some(idx)) = mesh.try_indices_option() {
                e.1 += idx.len() * 4;
            }
        }
        for (k, (n, b)) in by {
            cats.add(format!("Assets<Mesh> {k}"), n, b, false);
        }
    }
    probes::run(world, &mut cats);

    let accounted: u64 = cats.0.iter().map(|c| c.bytes).sum();
    let mut rows: Vec<Value> = cats
        .0
        .iter()
        .map(|c| json!({"name": c.name, "count": c.count, "bytes": c.bytes, "exact": c.exact}))
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r["bytes"].as_u64().unwrap_or(0)));
    let mut out = json!({
        "label": label,
        "t_s": state_started.elapsed().as_secs_f64(),
        "pid": std::process::id(),
        "process": proc,
        "live_heap": live,
        "peak_live_heap": peak.max(live.unwrap_or(0)),
        "accounted": accounted,
        "categories": rows,
    });
    if label == "in_match" || label == "final" {
        out["discovery"] = probes::discovery(world);
    }
    out
}
