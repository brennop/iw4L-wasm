//! `web-pack [--root DIR] [--cache-record FILE] [--image-cap PX] <record> <out.pack>`: turn an
//! `IW4L_FS_RECORD` log into a pack. Paths are stored relative to the games root
//! (`--root`, default `IW4L_GAMES`), so the pack works wherever the app mounts it.
//! `--cache-record` is an `IW4L_CACHE_RECORD` log; the artifact-cache entries it
//! names (wgsl, nav, localize) are stored zlib-compressed as ordinary pack files
//! under `.iw4l-cache/<kind>/<key>`, read back by `asset_transport::cache_get`.
//! `--image-cap PX` shrinks fully-read IWI textures in IWDs to at most PX on the
//! largest side by dropping top mips in place (see `web_pack_cap`).

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use gamefs::pack::{self, WriteFile};

use crate::dotenv::Env;
use crate::web_pack_cap::{self, Patches, Stats};

type Res<T> = Result<T, String>;

struct Seen {
    len: u64,
    ranges: Vec<(u64, u64)>,
}

fn merge(mut ranges: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    ranges.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

pub fn run(env: &Env, args: &[String]) -> Res<()> {
    let usage = "usage: web-pack [--root GAMES_ROOT] [--cache-record FILE] [--image-cap PX] <record> <out.pack>";
    let mut args = args;
    let mut root = None;
    let mut cache_record = None;
    let mut image_cap = None;
    while let [flag, value, rest @ ..] = args {
        match flag.as_str() {
            "--root" => root = Some(value.clone()),
            "--cache-record" => cache_record = Some(value.clone()),
            "--image-cap" => {
                image_cap = Some(
                    value
                        .parse::<u32>()
                        .map_err(|e| format!("--image-cap {value}: {e}"))?,
                );
            }
            _ => break,
        }
        args = rest;
    }
    let root = PathBuf::from(match root {
        Some(root) => root,
        None => env
            .get("IW4L_GAMES")
            .ok_or("web-pack: pass --root or set IW4L_GAMES")?,
    });
    let [record, out] = args else {
        return Err(usage.to_owned());
    };
    let log = std::fs::read_to_string(record).map_err(|e| format!("read {record}: {e}"))?;
    let mut seen: BTreeMap<PathBuf, Seen> = BTreeMap::new();
    let mut outside = Vec::new();
    for line in log.lines() {
        let mut parts = line.split('\t');
        if parts.next() != Some("R") {
            continue;
        }
        let (Some(path), Some(len), Some(start), Some(end)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let parse = |text: &str| text.parse::<u64>().map_err(|e| format!("{line}: {e}"));
        let Ok(relative) = Path::new(path).strip_prefix(&root) else {
            if !outside.iter().any(|seen: &String| seen == path) {
                outside.push(path.to_owned());
            }
            continue;
        };
        let entry = seen.entry(relative.to_path_buf()).or_insert(Seen {
            len: parse(len)?,
            ranges: Vec::new(),
        });
        entry.ranges.push((parse(start)?, parse(end)?));
    }
    if !outside.is_empty() {
        return Err(format!(
            "web-pack: {} recorded paths are outside the games root {}, e.g. {}",
            outside.len(),
            root.display(),
            outside[0]
        ));
    }
    let mut files = seen
        .into_iter()
        .map(|(path, seen)| WriteFile {
            path,
            len: seen.len,
            ranges: merge(seen.ranges),
        })
        .collect::<Vec<_>>();
    let mut patches: BTreeMap<PathBuf, Patches> = BTreeMap::new();
    let mut cap_stats = Stats::default();
    if let Some(cap) = image_cap {
        for file in &mut files {
            let is_iwd = file
                .path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("iwd"));
            if !is_iwd {
                continue;
            }
            let (patch, stats) = web_pack_cap::plan(&root.join(&file.path), &file.ranges, cap)?;
            file.ranges = web_pack_cap::subtract(&file.ranges, &patch.removed);
            cap_stats.add(&stats);
            patches.insert(file.path.clone(), patch);
        }
        println!(
            "web-pack: image cap {cap}: {} images capped ({} MiB -> {} MiB deflated); skipped: {} no mips, {} cube/volume/wavelet, {} unparsed, {} not fully read, {} already within cap",
            cap_stats.capped,
            cap_stats.before >> 20,
            cap_stats.after >> 20,
            cap_stats.no_mips,
            cap_stats.special,
            cap_stats.unparsed,
            cap_stats.not_covered,
            cap_stats.under_cap
        );
    }
    let baked = match &cache_record {
        Some(record) => bake_cache(record)?,
        None => BTreeMap::new(),
    };
    let baked_bytes: u64 = baked.values().map(|b| b.len() as u64).sum();
    for (path, bytes) in &baked {
        files.push(WriteFile {
            path: path.clone(),
            len: bytes.len() as u64,
            ranges: vec![(0, bytes.len() as u64)],
        });
    }
    let mut file = std::io::BufWriter::new(
        std::fs::File::create(out).map_err(|e| format!("create {out}: {e}"))?,
    );
    let total = pack::write(&mut file, &files, |path: &Path, start, buf: &mut [u8]| {
        if let Some(bytes) = baked.get(path) {
            buf.copy_from_slice(&bytes[start as usize..start as usize + buf.len()]);
            return Ok(());
        }
        let mut source = std::fs::File::open(root.join(path))?;
        source.seek(SeekFrom::Start(start))?;
        source.read_exact(buf)?;
        if let Some(patch) = patches.get(path) {
            web_pack_cap::apply(&patch.overlays, start, buf);
        }
        Ok(())
    })
    .map_err(|e| format!("write {out}: {e}"))?;
    let kept: u64 = files
        .iter()
        .filter(|f| !baked.contains_key(&f.path))
        .flat_map(|f| &f.ranges)
        .map(|(s, e)| e - s)
        .sum();
    let whole: u64 = files
        .iter()
        .filter(|f| !baked.contains_key(&f.path))
        .map(|f| f.len)
        .sum();
    println!(
        "web-pack: {} files, {} MiB of {} MiB, {} cache entries ({} MiB compressed), pack {} MiB -> {out}",
        files.len() - baked.len(),
        kept >> 20,
        whole >> 20,
        baked.len(),
        baked_bytes >> 20,
        total >> 20
    );
    Ok(())
}

/// Kinds worth shipping: mips are ~650 MiB decoded for mp_rust and save seconds.
/// Mirrors `asset_transport::PACK_CACHE_DIR` (xtask links no engine crates).
const PACK_CACHE_DIR: &str = ".iw4l-cache";

const PACKED_KINDS: [&str; 3] = ["wgsl", "nav", "localize"];

/// Reads the native cache entries an `IW4L_CACHE_RECORD` log names and returns
/// them compressed, keyed by their pack path. An entry the record names but the
/// cache lacks is an error: the recording run must leave every entry on disk.
fn bake_cache(record: &str) -> Res<BTreeMap<PathBuf, Vec<u8>>> {
    let log = std::fs::read_to_string(record).map_err(|e| format!("read {record}: {e}"))?;
    let wanted: BTreeSet<(&str, &str)> = log
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(kind, _)| PACKED_KINDS.contains(kind))
        .collect();
    let mut baked = BTreeMap::new();
    for (kind, key) in wanted {
        let source = Path::new("iw4l-artifacts")
            .join("cache")
            .join(kind)
            .join(&key[..2.min(key.len())])
            .join(key);
        let bytes =
            std::fs::read(&source).map_err(|e| format!("cache entry {}: {e}", source.display()))?;
        let packed = miniz_oxide::deflate::compress_to_vec_zlib(&bytes, 6);
        baked.insert(Path::new(PACK_CACHE_DIR).join(kind).join(key), packed);
    }
    Ok(baked)
}
