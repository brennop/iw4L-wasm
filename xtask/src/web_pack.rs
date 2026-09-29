//! `web-pack [--root DIR] <record> <out.pack>`: turn an `IW4L_FS_RECORD` log into
//! a pack. Paths are stored relative to the games root (`--root`, default
//! `IW4L_GAMES`), so the pack works wherever the app mounts it.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use gamefs::pack::{self, WriteFile};

use crate::dotenv::Env;

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
    let usage = "usage: web-pack [--root GAMES_ROOT] <record> <out.pack>";
    let (root, args) = match args {
        [flag, root, rest @ ..] if flag == "--root" => (root.clone(), rest),
        _ => (
            env.get("IW4L_GAMES")
                .ok_or("web-pack: pass --root or set IW4L_GAMES")?,
            args,
        ),
    };
    let root = PathBuf::from(root);
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
    let files = seen
        .into_iter()
        .map(|(path, seen)| WriteFile {
            path,
            len: seen.len,
            ranges: merge(seen.ranges),
        })
        .collect::<Vec<_>>();
    let mut file = std::io::BufWriter::new(
        std::fs::File::create(out).map_err(|e| format!("create {out}: {e}"))?,
    );
    let total = pack::write(&mut file, &files, |path: &Path, start, buf: &mut [u8]| {
        let mut source = std::fs::File::open(root.join(path))?;
        source.seek(SeekFrom::Start(start))?;
        source.read_exact(buf)
    })
    .map_err(|e| format!("write {out}: {e}"))?;
    let kept: u64 = files
        .iter()
        .flat_map(|f| &f.ranges)
        .map(|(s, e)| e - s)
        .sum();
    let whole: u64 = files.iter().map(|f| f.len).sum();
    println!(
        "web-pack: {} files, {} MiB of {} MiB, pack {} MiB -> {out}",
        files.len(),
        kept >> 20,
        whole >> 20,
        total >> 20
    );
    Ok(())
}
