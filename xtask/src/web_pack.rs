//! `web-pack <record> <out.pack>`: turn an `IW4L_FS_RECORD` log into a pack.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use gamefs::pack::{self, WriteFile};

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

pub fn run(args: &[String]) -> Res<()> {
    let [record, out] = args else {
        return Err("usage: web-pack <record> <out.pack>".to_owned());
    };
    let log = std::fs::read_to_string(record).map_err(|e| format!("read {record}: {e}"))?;
    let mut seen: BTreeMap<PathBuf, Seen> = BTreeMap::new();
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
        let entry = seen.entry(PathBuf::from(path)).or_insert(Seen {
            len: parse(len)?,
            ranges: Vec::new(),
        });
        entry.ranges.push((parse(start)?, parse(end)?));
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
        let mut source = std::fs::File::open(path)?;
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
