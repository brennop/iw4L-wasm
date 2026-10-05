//! `cargo xtask mem-census`: where the dedicated host's memory goes, in one
//! command. It builds a host with Bevy's type names, starts `xtask dedicated`
//! with `IW4L_MEM_CENSUS=1` and no joiner, waits for the in-match snapshot plus
//! `--secs`, asks the host for its final snapshot and the exact "drop" pass,
//! stops everything, and writes `summary.md` and `summary.csv`.
//! `--compare A B` diffs two such runs. `--help` has the rest.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::shell::{self, Res, Step};

const HELP: &str = "\
usage: cargo xtask mem-census [options]          (make mem-census MEM_CENSUS_ARGS='...')
       cargo xtask mem-census --compare DIR_A DIR_B

Runs one dedicated host (no joiner) with the in-process memory census on, then
writes summary.md and summary.csv. The host's own records are in mem-census.jsonl.

What it does: builds a host (bevy-debug feature, so resources have names, in its
own target dir), starts `cargo xtask dedicated` (master + host), waits for the
host to be in a match with its world spawned, stays --secs, then asks for a final
snapshot and the drop pass: every resource and match static is dropped in turn
and the fall in the counting allocator's live heap is that holder's exclusive
size. The host exits afterwards. Nothing is cut in the code; this only measures.

options:
  --map MAP          map the host serves (default mp_rust)
  --gpu              keep the renderer path (IW4L_DEDICATED_GPU=1); default is renderer-less
  --secs N           seconds to stay in the match before the final snapshot (default 120)
  --every N          seconds between periodic snapshots (default 30)
  --out DIR          output dir (default E:\\iw4l\\mem\\<timestamp>[-gpu] on Windows,
                     else target/mem-census/<timestamp>[-gpu])
  --drop-order O     forward (default; shared bytes are billed to the last holder dropped:
                     resources first, then the match statics) or reverse (statics first,
                     resources in reverse). Run both to see what is shared.
  --env KEY=VAL      extra environment for the host, repeatable (e.g. IW4L_NO_RESIDENT_MAP=1)
  --no-build         use the binaries already built (master, host, dist/web, pack)
  --compare A B      print a per-category diff of two run dirs (or their .jsonl files)
  -h, --help         this text

First build of the host takes several minutes (its own target dir); later ones are
incremental. Needs dist/web (cargo xtask web) and the pack, like `make dedicated`.

Columns: MB is exclusive bytes (freed by dropping just that holder); % is of the
final working set; exact = taken from the allocator, approx = modelled.
";

struct Args {
    map: String,
    gpu: bool,
    secs: u64,
    every: u64,
    out: Option<PathBuf>,
    reverse: bool,
    envs: Vec<(String, String)>,
    build: bool,
    compare: Option<(PathBuf, PathBuf)>,
}

fn parse(args: &[String]) -> Res<Option<Args>> {
    let mut out = Args {
        map: "mp_rust".into(),
        gpu: false,
        secs: 120,
        every: 30,
        out: None,
        reverse: false,
        envs: Vec::new(),
        build: true,
        compare: None,
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let mut value = |what: &str| -> Res<String> {
            iter.next()
                .cloned()
                .ok_or_else(|| format!("{arg} needs {what}"))
        };
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--map" => out.map = value("a map")?,
            "--gpu" => out.gpu = true,
            "--secs" => {
                out.secs = value("seconds")?
                    .parse()
                    .map_err(|_| "--secs needs a number".to_string())?;
            }
            "--every" => {
                out.every = value("seconds")?
                    .parse()
                    .map_err(|_| "--every needs a number".to_string())?;
            }
            "--out" => out.out = Some(PathBuf::from(value("a directory")?)),
            "--drop-order" => match value("forward or reverse")?.as_str() {
                "forward" => out.reverse = false,
                "reverse" => out.reverse = true,
                other => return Err(format!("--drop-order is forward or reverse, not {other}")),
            },
            "--env" => {
                let kv = value("KEY=VAL")?;
                let (k, v) = kv
                    .split_once('=')
                    .ok_or_else(|| "--env needs KEY=VAL".to_string())?;
                out.envs.push((k.to_string(), v.to_string()));
            }
            "--no-build" => out.build = false,
            "--compare" => {
                let a = PathBuf::from(value("two run dirs")?);
                let b = PathBuf::from(value("two run dirs")?);
                out.compare = Some((a, b));
            }
            other => return Err(format!("unknown option {other}; see --help")),
        }
    }
    Ok(Some(out))
}

pub fn run_cli(root: &Path, args: &[String]) -> Res<()> {
    let Some(args) = parse(args)? else {
        print!("{HELP}");
        return Ok(());
    };
    if let Some((a, b)) = &args.compare {
        let text = compare(&load(a)?, &load(b)?, a, b);
        print!("{text}");
        return Ok(());
    }
    run(root, &args)
}

fn mem_target_dir(root: &Path) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(r"E:\iw4l\mem\target")
    } else {
        root.join("target/mem-census")
    }
}

fn exe(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

fn git(root: &Path, args: &[&str]) -> String {
    Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn records(dir: &Path) -> Vec<Value> {
    fs::read_to_string(dir.join("mem-census.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn has_label(dir: &Path, label: &str) -> bool {
    records(dir).iter().any(|r| r["label"] == label)
}

fn run(root: &Path, args: &Args) -> Res<()> {
    shell::require_tools(&["cargo"])?;
    let target = mem_target_dir(root);
    let host_exe = exe(&target.join("play"), "iw4l");
    if args.build {
        let step = Step::start("mem-census.build", "iw4l-master (debug)");
        shell::run(
            Command::new("cargo")
                .current_dir(root)
                .args(["build", "-p", "iw4l-master"]),
        )?;
        step.done("");
        let step = Step::start(
            "mem-census.build",
            "host with Bevy type names (play profile)",
        );
        shell::run(
            Command::new("cargo")
                .current_dir(root)
                .args([
                    "build",
                    "--profile",
                    "play",
                    "-p",
                    "launcher",
                    "--features",
                    "bevy-debug",
                ])
                .arg("--target-dir")
                .arg(&target),
        )?;
        step.done("");
    }
    if !host_exe.is_file() {
        return Err(format!(
            "host binary missing: {} (run without --no-build)",
            host_exe.display()
        ));
    }
    let out = match &args.out {
        Some(dir) => dir.clone(),
        None => {
            let base = if cfg!(windows) {
                PathBuf::from(r"E:\iw4l\mem")
            } else {
                root.join("target/mem-census")
            };
            base.join(format!(
                "{}{}",
                crate::dedicated::timestamp(),
                if args.gpu { "-gpu" } else { "" }
            ))
        }
    };
    fs::create_dir_all(&out).map_err(|e| format!("create {}: {e}", out.display()))?;
    let out = std::path::absolute(&out).map_err(|e| e.to_string())?;
    let _ = fs::remove_file(out.join("mem-census.jsonl"));
    let _ = fs::remove_file(out.join("mem-census.now"));
    let stop = out.join("stop");
    let _ = fs::remove_file(&stop);

    let dirty = !git(root, &["status", "--porcelain", "--untracked-files=no"]).is_empty();
    let meta = json!({
        "commit": git(root, &["rev-parse", "--short", "HEAD"]),
        "dirty": dirty,
        "map": args.map,
        "gpu": args.gpu,
        "secs": args.secs,
        "every": args.every,
        "drop_order": if args.reverse { "reverse" } else { "forward" },
        "env": args.envs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>(),
        "command": std::env::args().skip(1).collect::<Vec<_>>().join(" "),
    });
    fs::write(
        out.join("meta.json"),
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    )
    .map_err(|e| e.to_string())?;
    println!("mem-census: out {}", out.display());

    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(me);
    cmd.current_dir(root)
        .arg("dedicated")
        .args([
            "--bind",
            "127.0.0.1",
            "--name",
            "mem-census",
            "--no-build",
            "--no-web",
        ])
        .args(["--map", &args.map])
        .arg("--run-dir")
        .arg(out.join("run"))
        .arg("--stop-file")
        .arg(&stop)
        .arg("--host-exe")
        .arg(&host_exe)
        .env("IW4L_MEM_CENSUS", "1")
        .env("IW4L_COUNTING_ALLOC", "1")
        .env("IW4L_MEM_CENSUS_DROP", "1")
        .env("IW4L_MEM_CENSUS_DIR", &out)
        .env("IW4L_MEM_CENSUS_EVERY_S", args.every.to_string())
        .env(
            "IW4L_MEM_CENSUS_DROP_ORDER",
            if args.reverse { "reverse" } else { "forward" },
        )
        .env_remove("IW4L_RUN_DIR");
    if args.gpu {
        cmd.env("IW4L_DEDICATED_GPU", "1");
    } else {
        cmd.env_remove("IW4L_DEDICATED_GPU");
    }
    for (k, v) in &args.envs {
        cmd.env(k, v);
    }
    let log = fs::File::create(out.join("xtask-dedicated.out")).map_err(|e| e.to_string())?;
    let log_err = log.try_clone().map_err(|e| e.to_string())?;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err)
        .spawn()
        .map_err(|e| format!("start dedicated: {e}"))?;
    println!("mem-census: dedicated launcher pid {}", child.id());

    let result = drive(&mut child, &out, &stop, args);
    // Ask for a clean stop, then make sure (by PID, only our own child).
    let _ = fs::write(&stop, b"stop");
    let until = Instant::now() + Duration::from_secs(30);
    while Instant::now() < until {
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    if matches!(child.try_wait(), Ok(None)) {
        let _ = child.kill();
        let _ = child.wait();
    }
    result?;

    let report = load(&out)?;
    let md = summary_md(&report);
    fs::write(out.join("summary.md"), &md).map_err(|e| e.to_string())?;
    fs::write(out.join("summary.csv"), summary_csv(&report)).map_err(|e| e.to_string())?;
    print!("{md}");
    println!(
        "mem-census: wrote {} and summary.csv",
        out.join("summary.md").display()
    );
    Ok(())
}

fn drive(child: &mut std::process::Child, out: &Path, _stop: &Path, args: &Args) -> Res<()> {
    let alive = |child: &mut std::process::Child| matches!(child.try_wait(), Ok(None));
    let wait_for = |child: &mut std::process::Child, label: &str, limit: u64| -> Res<()> {
        let until = Instant::now() + Duration::from_secs(limit);
        while Instant::now() < until {
            if has_label(out, label) {
                return Ok(());
            }
            if !alive(child) {
                return Err(format!(
                    "the launcher exited before the `{label}` snapshot; see {}",
                    out.join("xtask-dedicated.out").display()
                ));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        Err(format!(
            "no `{label}` snapshot within {limit}s; see {}",
            out.join("run").display()
        ))
    };
    println!("mem-census: waiting for the host to be in a match with its world spawned");
    wait_for(child, "in_match", 600)?;
    println!("mem-census: in match; staying {}s", args.secs);
    let until = Instant::now() + Duration::from_secs(args.secs);
    while Instant::now() < until {
        if !alive(child) {
            return Err("the launcher exited while waiting in the match".into());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    fs::write(out.join("mem-census.now"), b"now").map_err(|e| e.to_string())?;
    println!("mem-census: final snapshot and drop pass");
    wait_for(child, "drop", 120)
}

// ---------------------------------------------------------------- report

struct Row {
    name: String,
    count: Option<u64>,
    bytes: i64,
    exact: bool,
}

struct Report {
    meta: Value,
    snaps: Vec<Value>,
    drop: Option<Value>,
    rows: Vec<Row>,
    ws: i64,
    live: i64,
    residual: i64,
}

fn load(path: &Path) -> Res<Report> {
    let dir = if path.is_file() {
        path.parent().unwrap_or(Path::new(".")).to_path_buf()
    } else {
        path.to_path_buf()
    };
    let recs = if path.is_file() {
        fs::read_to_string(path)
            .map_err(|e| e.to_string())?
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .collect()
    } else {
        records(&dir)
    };
    if recs.is_empty() {
        return Err(format!("no mem-census.jsonl records in {}", dir.display()));
    }
    let meta = fs::read_to_string(dir.join("meta.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    let drop = recs.iter().find(|r| r["label"] == "drop").cloned();
    let snaps: Vec<Value> = recs.into_iter().filter(|r| r["label"] != "drop").collect();
    let last = snaps
        .iter()
        .rfind(|r| r["label"] == "final")
        .or(snaps.last())
        .ok_or("no snapshots")?;
    let ws = last["process"]["working_set"].as_i64().unwrap_or(0);
    let mut rows = Vec::new();
    let (mut live, mut residual) = (last["live_heap"].as_i64().unwrap_or(0), 0);
    if let Some(d) = &drop {
        let pairs = |key: &str| -> Vec<(String, i64)> {
            d[key]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|p| {
                            (
                                p[0].as_str().unwrap_or("?").to_string(),
                                p[1].as_i64().unwrap_or(0),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        for (name, bytes) in pairs("resources_top")
            .into_iter()
            .chain(pairs("statics"))
            .chain(pairs("worldscene_fields"))
        {
            rows.push(Row {
                name,
                count: None,
                bytes,
                exact: true,
            });
        }
        let beyond = d["resources_freed_beyond_top120"].as_i64().unwrap_or(0);
        if beyond != 0 {
            rows.push(Row {
                name: "resources beyond the top 120 (summed)".into(),
                count: None,
                bytes: beyond,
                exact: true,
            });
        }
        rows.push(Row {
            name: "entities and their components (all dropped)".into(),
            count: None,
            bytes: d["entities_freed"].as_i64().unwrap_or(0),
            exact: true,
        });
        live = d["live_before"].as_i64().unwrap_or(live);
        residual = d["live_after_entities"].as_i64().unwrap_or(0);
    } else {
        for c in last["categories"].as_array().cloned().unwrap_or_default() {
            rows.push(Row {
                name: c["name"].as_str().unwrap_or("?").to_string(),
                count: c["count"].as_u64(),
                bytes: c["bytes"].as_i64().unwrap_or(0),
                exact: c["exact"].as_bool().unwrap_or(false),
            });
        }
    }
    rows.retain(|r| r.bytes != 0);
    rows.sort_by(|a, b| b.bytes.cmp(&a.bytes));
    Ok(Report {
        meta,
        snaps,
        drop,
        rows,
        ws,
        live,
        residual,
    })
}

fn mb(bytes: i64) -> String {
    format!("{:.1}", bytes as f64 / 1_048_576.0)
}

fn pct(bytes: i64, ws: i64) -> String {
    if ws <= 0 {
        "-".into()
    } else {
        format!("{:.1}", bytes as f64 * 100.0 / ws as f64)
    }
}

fn exact_word(exact: bool) -> &'static str {
    if exact { "exact" } else { "approx" }
}

fn named_total(r: &Report) -> i64 {
    r.rows.iter().map(|row| row.bytes).sum()
}

fn summary_md(r: &Report) -> String {
    let mut s = String::new();
    let m = &r.meta;
    s.push_str("# Memory census\n\n");
    s.push_str(&format!(
        "commit `{}`{}, map `{}`, path `{}`, stayed {}s, drop order {}, extra env [{}]\n\n",
        m["commit"].as_str().unwrap_or("?"),
        if m["dirty"] == true {
            " (+uncommitted changes)"
        } else {
            ""
        },
        m["map"].as_str().unwrap_or("?"),
        if m["gpu"] == true {
            "renderer (IW4L_DEDICATED_GPU=1)"
        } else {
            "renderer-less"
        },
        m["secs"],
        m["drop_order"].as_str().unwrap_or("?"),
        m["env"]
            .as_array()
            .map(|a| a
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "))
            .unwrap_or_default(),
    ));
    s.push_str("## Process numbers per milestone\n\n");
    s.push_str("| milestone | t s | working set MB | peak WS MB | private MB | live heap MB | peak live MB |\n|---|---|---|---|---|---|---|\n");
    for snap in &r.snaps {
        let p = &snap["process"];
        let g = |v: &Value| v.as_i64().map_or("-".to_string(), mb);
        s.push_str(&format!(
            "| {} | {:.0} | {} | {} | {} | {} | {} |\n",
            snap["label"].as_str().unwrap_or("?"),
            snap["t_s"].as_f64().unwrap_or(0.0),
            g(&p["working_set"]),
            g(&p["peak_working_set"]),
            g(&p["private_bytes"]),
            g(&snap["live_heap"]),
            g(&snap["peak_live_heap"]),
        ));
    }
    if r.snaps.iter().all(|x| x["live_heap"].is_null()) {
        s.push_str(
            "\nLive heap is off (IW4L_COUNTING_ALLOC unset), so the drop pass has no numbers.\n",
        );
    }
    if let Some(last) = r.snaps.last() {
        let p = &last["process"];
        let extra: Vec<String> = p
            .as_object()
            .map(|o| {
                o.iter()
                    .filter(|(k, v)| {
                        !matches!(
                            k.as_str(),
                            "working_set" | "peak_working_set" | "private_bytes"
                        ) && v.is_number()
                    })
                    .map(|(k, v)| format!("{k}={}", v.as_i64().map_or("-".into(), mb)))
                    .collect()
            })
            .unwrap_or_default();
        s.push_str(&format!(
            "\nLast snapshot, other process figures (MB): {}\n",
            extra.join(", ")
        ));
    }
    s.push_str("\n## Holders, largest first\n\n");
    if r.drop.is_none() {
        s.push_str(
            "No drop record: showing the non-destructive categories of the last snapshot only.\n\n",
        );
    }
    s.push_str(&format!(
        "Exclusive bytes per holder ({} order); the rows add up to the live heap before the drop ({} MB).\n\n",
        m["drop_order"].as_str().unwrap_or("?"),
        mb(r.live)
    ));
    s.push_str("| category | count | MB | % of working set | exact? |\n|---|---|---|---|---|\n");
    let mut small = 0i64;
    let mut small_n = 0;
    for row in &r.rows {
        if row.bytes < 512 * 1024 {
            small += row.bytes;
            small_n += 1;
            continue;
        }
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            row.name,
            row.count.map_or("-".into(), |c| c.to_string()),
            mb(row.bytes),
            pct(row.bytes, r.ws),
            exact_word(row.exact)
        ));
    }
    if small_n > 0 {
        s.push_str(&format!(
            "| {small_n} rows under 0.5 MB each | - | {} | {} | exact |\n",
            mb(small),
            pct(small, r.ws)
        ));
    }
    let named = named_total(r);
    let unattributed = r.residual;
    s.push_str(&format!(
        "| live heap held by nothing above (statics outside the match, thread-locals, Bevy internals) | - | {} | {} | exact |\n",
        mb(unattributed),
        pct(unattributed, r.ws)
    ));
    let non_heap = r.ws - r.live;
    s.push_str(&format!(
        "| not heap: working set minus live heap (allocator slack, thread stacks, code, mapped files, driver) | - | {} | {} | derived |\n",
        mb(non_heap),
        pct(non_heap, r.ws)
    ));
    s.push_str(&format!(
        "| **working set (final snapshot)** | - | {} | 100.0 | exact |\n\n",
        mb(r.ws)
    ));
    let unacc = r.ws - named;
    s.push_str(&format!(
        "**Unaccounted** (working set minus every named holder above) = {} MB = {}% of the working set: {} MB of live heap held by nothing named, plus {} MB that is not heap.\n\n",
        mb(unacc),
        pct(unacc, r.ws),
        mb(unattributed),
        mb(non_heap)
    ));
    if let Some(private) = r
        .snaps
        .last()
        .and_then(|x| x["process"]["private_bytes"].as_i64())
        && private > 0
    {
        s.push_str(&format!(
            "Private bytes (committed) were {} MB, {} MB {} the working set. The working set leaves out committed pages the OS trimmed, so size a box against private bytes (Windows) or RssAnon (Linux), not the working set alone.\n\n",
            mb(private),
            mb((private - r.ws).abs()),
            if private >= r.ws { "above" } else { "below" }
        ));
    }
    if let Some(last) = r
        .snaps
        .iter()
        .rfind(|x| x["label"] == "final")
        .or(r.snaps.last())
    {
        s.push_str("## Breakdowns from the final snapshot (inside rows above, not additive)\n\n| category | count | MB | exact? |\n|---|---|---|---|\n");
        for c in last["categories"].as_array().cloned().unwrap_or_default() {
            s.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                c["name"].as_str().unwrap_or("?"),
                c["count"],
                mb(c["bytes"].as_i64().unwrap_or(0)),
                exact_word(c["exact"].as_bool().unwrap_or(false))
            ));
        }
        s.push('\n');
    }
    s.push_str("Sharing: bytes held through an `Arc` by two holders are billed to the one dropped last. `--drop-order reverse` drops the match statics first; compare the two runs to see what is shared.\n");
    s
}

fn summary_csv(r: &Report) -> String {
    let mut s = String::from("category,count,bytes,mb,pct_of_working_set,exact\n");
    let mut line = |name: &str, count: Option<u64>, bytes: i64, exact: &str| {
        s.push_str(&format!(
            "\"{}\",{},{},{},{},{}\n",
            name.replace('"', "'"),
            count.map_or(String::new(), |c| c.to_string()),
            bytes,
            mb(bytes),
            pct(bytes, r.ws),
            exact
        ));
    };
    for row in &r.rows {
        line(&row.name, row.count, row.bytes, exact_word(row.exact));
    }
    line("live heap held by nothing named", None, r.residual, "exact");
    line(
        "not heap (working set - live heap)",
        None,
        r.ws - r.live,
        "derived",
    );
    line("working set (final)", None, r.ws, "exact");
    line(
        "unaccounted (working set - named holders)",
        None,
        r.ws - named_total(r),
        "derived",
    );
    s
}

fn compare(a: &Report, b: &Report, da: &Path, db: &Path) -> String {
    let side = |r: &Report| -> BTreeMap<String, i64> {
        let mut m: BTreeMap<String, i64> = BTreeMap::new();
        for row in &r.rows {
            *m.entry(row.name.clone()).or_default() += row.bytes;
        }
        m.insert("live heap held by nothing named".into(), r.residual);
        m.insert("not heap (working set - live heap)".into(), r.ws - r.live);
        m.insert("working set (final)".into(), r.ws);
        m
    };
    let (ma, mb_) = (side(a), side(b));
    let mut names: Vec<&String> = ma.keys().chain(mb_.keys()).collect();
    names.sort();
    names.dedup();
    let mut rows: Vec<(String, i64, i64)> = names
        .into_iter()
        .map(|n| {
            (
                n.clone(),
                *ma.get(n).unwrap_or(&0),
                *mb_.get(n).unwrap_or(&0),
            )
        })
        .filter(|(_, x, y)| x.abs() >= 512 * 1024 || y.abs() >= 512 * 1024)
        .collect();
    rows.sort_by_key(|(_, x, y)| std::cmp::Reverse((y - x).abs()));
    let desc = |r: &Report, d: &Path| {
        format!(
            "{} ({} {}, {})",
            d.display(),
            r.meta["commit"].as_str().unwrap_or("?"),
            if r.meta["gpu"] == true {
                "gpu"
            } else {
                "renderer-less"
            },
            r.meta["drop_order"].as_str().unwrap_or("?")
        )
    };
    let mut s = format!(
        "# Memory census compare\n\nA = {}\nB = {}\n\n",
        desc(a, da),
        desc(b, db)
    );
    s.push_str("| category | A MB | B MB | B - A MB | B vs A % |\n|---|---|---|---|---|\n");
    for (name, x, y) in &rows {
        let rel = if *x != 0 {
            format!("{:+.1}", (*y - *x) as f64 * 100.0 / *x as f64)
        } else {
            "new".into()
        };
        s.push_str(&format!(
            "| {name} | {} | {} | {:+.1} | {rel} |\n",
            mb(*x),
            mb(*y),
            (*y - *x) as f64 / 1_048_576.0
        ));
    }
    s.push_str(
        "\nRows under 0.5 MB on both sides are left out. Sorted by the size of the change.\n",
    );
    s
}
