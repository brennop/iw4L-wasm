//! `web-pack --image-cap`: shrink `images/*.iwi` entries inside IWD (zip) archives by
//! dropping their top mip levels, patched in place so the pack format and the engine
//! stay as they are. An IWI stores mips smallest first, so dropping `k` levels is a
//! truncation plus a header rewrite (dims >> k, picmip table shifted). The entry's
//! new raw-deflate data is written at its original data offset; compressed size,
//! uncompressed size and CRC-32 are patched in the local header and the central
//! directory record, and the bytes between the new and the old data end leave the
//! recorded ranges. No other offset moves.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

type Res<T> = Result<T, String>;

const IWI_HEADER_LEN: usize = 32;
const LOCAL_HEADER_LEN: u64 = 30;

/// What to change in one archive: bytes overlaid at absolute offsets and byte ranges
/// dropped from the recorded ones.
#[derive(Default)]
pub struct Patches {
    pub overlays: Vec<(u64, Vec<u8>)>,
    pub removed: Vec<(u64, u64)>,
}

#[derive(Default)]
pub struct Stats {
    pub capped: usize,
    pub before: u64,
    pub after: u64,
    pub not_covered: usize,
    pub no_mips: usize,
    pub special: usize,
    pub unparsed: usize,
    pub under_cap: usize,
}

impl Stats {
    pub fn add(&mut self, other: &Stats) {
        self.capped += other.capped;
        self.before += other.before;
        self.after += other.after;
        self.not_covered += other.not_covered;
        self.no_mips += other.no_mips;
        self.special += other.special;
        self.unparsed += other.unparsed;
        self.under_cap += other.under_cap;
    }
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn covered(ranges: &[(u64, u64)], start: u64, end: u64) -> bool {
    ranges.iter().any(|&(s, e)| s <= start && end <= e)
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, slot) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *slot = c;
    }
    !data.iter().fold(!0u32, |c, &b| {
        table[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8)
    })
}

/// Bytes of one mip level, mirroring `compressed_mip_bytes` in `asset_material`.
fn mip_bytes(width: usize, height: usize, format: u8) -> Option<usize> {
    let blocks = width.div_ceil(4) * height.div_ceil(4);
    Some(match format {
        1 => width * height * 4,
        2 => width * height * 3,
        3 => width * height * 2,
        4 | 5 => width * height,
        11 => blocks * 8,
        12 | 13 => blocks * 16,
        _ => return None,
    })
}

pub enum Trim {
    Done(Vec<u8>),
    NoMips,
    Special,
    Unparsed,
    UnderCap,
}

/// Drops top mips of an inflated IWI until its largest side is at most `cap`.
pub fn trim_iwi(bytes: &[u8], cap: u32) -> Trim {
    if bytes.len() < IWI_HEADER_LEN || bytes[..3] != *b"IWi" || bytes[3] != 8 {
        return Trim::Unparsed;
    }
    let (flags, usage, format) = (bytes[4], bytes[6], bytes[8]);
    let (width, height, depth) = (
        usize::from(u16_at(bytes, 10)),
        usize::from(u16_at(bytes, 12)),
        u16_at(bytes, 14),
    );
    let side = width.max(height);
    if side as u32 <= cap {
        return Trim::UnderCap;
    }
    if flags & 0x2 != 0 {
        return Trim::NoMips;
    }
    if depth > 1 || matches!(usage, 1 | 9) || (6..=10).contains(&format) {
        return Trim::Special;
    }
    if width == 0 || height == 0 || mip_bytes(1, 1, format).is_none() {
        return Trim::Unparsed;
    }
    let levels = (usize::BITS - side.leading_zeros()) as usize;
    let level_bytes = |l: usize| mip_bytes((width >> l).max(1), (height >> l).max(1), format);
    // The file must be exactly the header plus every level, or the layout is not ours.
    let mut total = IWI_HEADER_LEN;
    for l in 0..levels {
        match level_bytes(l) {
            Some(n) => total += n,
            None => return Trim::Unparsed,
        }
    }
    if total != bytes.len() {
        return Trim::Unparsed;
    }
    let mut k = 0;
    while (side >> k) as u32 > cap {
        k += 1;
    }
    if k >= levels {
        return Trim::Unparsed;
    }
    let mut kept = IWI_HEADER_LEN;
    for l in k..levels {
        kept += level_bytes(l).unwrap_or(0);
    }
    // Table entry i is the file length with i levels removed (from the new top).
    let mut table = [0u32; 4];
    for (i, slot) in table.iter_mut().enumerate() {
        let mut size = IWI_HEADER_LEN;
        for l in k + i..levels {
            size += level_bytes(l).unwrap_or(0);
        }
        *slot = size as u32;
    }
    let mut out = bytes[..kept].to_vec();
    out[10..12].copy_from_slice(&((width >> k).max(1) as u16).to_le_bytes());
    out[12..14].copy_from_slice(&((height >> k).max(1) as u16).to_le_bytes());
    for (i, v) in table.iter().enumerate() {
        out[16 + i * 4..20 + i * 4].copy_from_slice(&v.to_le_bytes());
    }
    Trim::Done(out)
}

struct Entry {
    name: String,
    method: u16,
    csize: u32,
    local: u64,
    central: u64,
}

/// Central directory records of a non-zip64 archive with their absolute offsets.
fn read_directory(file: &mut std::fs::File, len: u64) -> Res<Vec<Entry>> {
    let tail_len = len.min(22 + 65535);
    let mut tail = vec![0u8; tail_len as usize];
    file.seek(SeekFrom::Start(len - tail_len))
        .and_then(|_| file.read_exact(&mut tail))
        .map_err(|e| e.to_string())?;
    let eocd = (0..=tail.len().saturating_sub(22))
        .rev()
        .find(|&i| tail[i..i + 4] == *b"PK\x05\x06")
        .ok_or("no end of central directory")?;
    let count = usize::from(u16_at(&tail, eocd + 10));
    let (size, offset) = (
        u64::from(u32_at(&tail, eocd + 12)),
        u64::from(u32_at(&tail, eocd + 16)),
    );
    if offset == 0xFFFF_FFFF || count == 0xFFFF || offset + size > len {
        return Err("zip64 or bad directory".into());
    }
    let mut dir = vec![0u8; size as usize];
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| file.read_exact(&mut dir))
        .map_err(|e| e.to_string())?;
    let mut entries = Vec::with_capacity(count);
    let mut at = 0usize;
    while at + 46 <= dir.len() && dir[at..at + 4] == *b"PK\x01\x02" {
        let (n, m, c) = (
            usize::from(u16_at(&dir, at + 28)),
            usize::from(u16_at(&dir, at + 30)),
            usize::from(u16_at(&dir, at + 32)),
        );
        let name = dir.get(at + 46..at + 46 + n).ok_or("truncated directory")?;
        let flags = u16_at(&dir, at + 8);
        let (csize, usize_, local) = (
            u32_at(&dir, at + 20),
            u32_at(&dir, at + 24),
            u32_at(&dir, at + 42),
        );
        // Bit 0 encrypted, bit 3 data descriptor; zip64 markers: leave alone.
        if flags & 0x9 == 0 && csize != 0xFFFF_FFFF && usize_ != 0xFFFF_FFFF && local != 0xFFFF_FFFF
        {
            entries.push(Entry {
                name: String::from_utf8_lossy(name).to_lowercase(),
                method: u16_at(&dir, at + 10),
                csize,
                local: u64::from(local),
                central: offset + at as u64,
            });
        }
        at += 46 + n + m + c;
    }
    Ok(entries)
}

/// Patches for one IWD whose recorded ranges are `ranges`.
pub fn plan(path: &Path, ranges: &[(u64, u64)], cap: u32) -> Res<(Patches, Stats)> {
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let len = file.metadata().map_err(|e| e.to_string())?.len();
    let entries = read_directory(&mut file, len).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut patches = Patches::default();
    let mut stats = Stats::default();
    for entry in entries {
        if !(entry.name.starts_with("images/") && entry.name.ends_with(".iwi"))
            || !matches!(entry.method, 0 | 8)
        {
            continue;
        }
        let mut head = [0u8; LOCAL_HEADER_LEN as usize];
        if !covered(ranges, entry.local, entry.local + LOCAL_HEADER_LEN) {
            stats.not_covered += 1;
            continue;
        }
        if file
            .seek(SeekFrom::Start(entry.local))
            .and_then(|_| file.read_exact(&mut head))
            .is_err()
            || head[..4] != *b"PK\x03\x04"
        {
            stats.unparsed += 1;
            continue;
        }
        let data = entry.local
            + LOCAL_HEADER_LEN
            + u64::from(u16_at(&head, 26))
            + u64::from(u16_at(&head, 28));
        let end = data + u64::from(entry.csize);
        if !covered(ranges, data, end) || !covered(ranges, entry.central, entry.central + 46) {
            stats.not_covered += 1;
            continue;
        }
        let mut raw = vec![0u8; entry.csize as usize];
        if file
            .seek(SeekFrom::Start(data))
            .and_then(|_| file.read_exact(&mut raw))
            .is_err()
        {
            stats.unparsed += 1;
            continue;
        }
        let inflated = if entry.method == 8 {
            match miniz_oxide::inflate::decompress_to_vec_with_limit(&raw, 1 << 28) {
                Ok(bytes) => bytes,
                Err(_) => {
                    stats.unparsed += 1;
                    continue;
                }
            }
        } else {
            raw
        };
        let trimmed = match trim_iwi(&inflated, cap) {
            Trim::Done(bytes) => bytes,
            Trim::NoMips => {
                stats.no_mips += 1;
                continue;
            }
            Trim::Special => {
                stats.special += 1;
                continue;
            }
            Trim::Unparsed => {
                stats.unparsed += 1;
                continue;
            }
            Trim::UnderCap => {
                stats.under_cap += 1;
                continue;
            }
        };
        let packed = if entry.method == 8 {
            miniz_oxide::deflate::compress_to_vec(&trimmed, 9)
        } else {
            trimmed.clone()
        };
        if packed.len() as u64 >= u64::from(entry.csize) {
            stats.unparsed += 1;
            continue;
        }
        let (crc, csize, usize_) = (
            crc32(&trimmed).to_le_bytes(),
            (packed.len() as u32).to_le_bytes(),
            (trimmed.len() as u32).to_le_bytes(),
        );
        let fields = |crc_at: u64| {
            let mut bytes = Vec::with_capacity(12);
            bytes.extend_from_slice(&crc);
            bytes.extend_from_slice(&csize);
            bytes.extend_from_slice(&usize_);
            (crc_at, bytes)
        };
        patches.overlays.push(fields(entry.local + 14));
        patches.overlays.push(fields(entry.central + 16));
        patches.removed.push((data + packed.len() as u64, end));
        stats.capped += 1;
        stats.before += u64::from(entry.csize);
        stats.after += packed.len() as u64;
        patches.overlays.push((data, packed));
    }
    Ok((patches, stats))
}

/// `ranges` without the `removed` spans (which need not be sorted or disjoint from each other).
pub fn subtract(ranges: &[(u64, u64)], removed: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut removed = removed.to_vec();
    removed.sort_unstable();
    let mut out = Vec::new();
    for &(mut start, end) in ranges {
        for &(rs, re) in &removed {
            if re <= start || rs >= end {
                continue;
            }
            if rs > start {
                out.push((start, rs));
            }
            start = start.max(re);
        }
        if start < end {
            out.push((start, end));
        }
    }
    out
}

/// Applies the overlays that intersect the window `[start, start + buf.len())`.
pub fn apply(overlays: &[(u64, Vec<u8>)], start: u64, buf: &mut [u8]) {
    let end = start + buf.len() as u64;
    for (at, bytes) in overlays {
        let (from, to) = ((*at).max(start), (*at + bytes.len() as u64).min(end));
        if from < to {
            buf[(from - start) as usize..(to - start) as usize]
                .copy_from_slice(&bytes[(from - at) as usize..(to - at) as usize]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A DXT1 IWI with every mip level down to 1x1; level bytes are a running counter.
    fn iwi(side: usize, format: u8) -> Vec<u8> {
        let levels = (usize::BITS - side.leading_zeros()) as usize;
        let sizes: Vec<usize> = (0..levels)
            .map(|l| mip_bytes((side >> l).max(1), (side >> l).max(1), format).unwrap())
            .collect();
        let mut out = vec![0u8; IWI_HEADER_LEN];
        out[..4].copy_from_slice(b"IWi\x08");
        out[8] = format;
        out[10..12].copy_from_slice(&(side as u16).to_le_bytes());
        out[12..14].copy_from_slice(&(side as u16).to_le_bytes());
        out[14..16].copy_from_slice(&1u16.to_le_bytes());
        let total: usize = IWI_HEADER_LEN + sizes.iter().sum::<usize>();
        let mut size = total;
        for i in 0..4 {
            out[16 + i * 4..20 + i * 4].copy_from_slice(&(size as u32).to_le_bytes());
            size -= sizes.get(i).copied().unwrap_or(0);
        }
        let mut state = 0x2545_F491u32;
        out.extend((0..total - IWI_HEADER_LEN).map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 24) as u8
        }));
        out
    }

    /// The engine's walk from the header offset down to the header length.
    fn walk(bytes: &[u8], format: u8) -> (usize, usize) {
        let (w, h) = (
            usize::from(u16_at(bytes, 10)),
            usize::from(u16_at(bytes, 12)),
        );
        let mut cursor = u32_at(bytes, 16) as usize;
        assert_eq!(cursor, bytes.len());
        let levels = (usize::BITS - w.max(h).leading_zeros()) as usize;
        for l in 0..levels {
            cursor -= mip_bytes((w >> l).max(1), (h >> l).max(1), format).unwrap();
        }
        (cursor, w)
    }

    fn zip_with(entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
        let mut cursor = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(&mut cursor);
        for (name, data, deflate) in entries {
            let method = if *deflate {
                zip::CompressionMethod::Deflated
            } else {
                zip::CompressionMethod::Stored
            };
            zip.start_file(
                *name,
                zip::write::FileOptions::<()>::default().compression_method(method),
            )
            .unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
        cursor.into_inner()
    }

    #[test]
    fn crc_matches_zip() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn trims_and_patches_zip() {
        for (format, cap, want) in [(11u8, 8usize, 8usize), (1, 8, 8), (13, 16, 16)] {
            let big = iwi(32, format);
            let other = iwi(8, format);
            let bytes = zip_with(&[
                ("images/big.iwi", &big, true),
                ("images/stored.iwi", &big, false),
                ("images/small.iwi", &other, true),
                ("sound/x.wav", b"hello", true),
            ]);
            let dir = std::env::temp_dir().join(format!("iwi-cap-{}-{format}", std::process::id()));
            std::fs::write(&dir, &bytes).unwrap();
            let all = [(0, bytes.len() as u64)];
            let (patches, stats) = plan(&dir, &all, cap as u32).unwrap();
            std::fs::remove_file(&dir).unwrap();
            assert_eq!((stats.capped, stats.under_cap), (2, 1));

            let mut patched = bytes.clone();
            for (at, data) in &patches.overlays {
                patched[*at as usize..*at as usize + data.len()].copy_from_slice(data);
            }
            // Removed bytes are never read: poison them.
            let kept = subtract(&all, &patches.removed);
            for (s, e) in &patches.removed {
                patched[*s as usize..*e as usize].fill(0xAA);
            }
            assert!(kept.len() > 1);
            let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&patched)).unwrap();
            for name in ["images/big.iwi", "images/stored.iwi"] {
                let mut out = Vec::new();
                zip.by_name(name).unwrap().read_to_end(&mut out).unwrap(); // verifies CRC
                let (cursor, w) = walk(&out, format);
                assert_eq!((cursor, w), (IWI_HEADER_LEN, want));
                let table: Vec<u32> = (0..4).map(|i| u32_at(&out, 16 + i * 4)).collect();
                assert_eq!(table[0] as usize, out.len());
                assert!(table.windows(2).all(|p| p[0] >= p[1]));
                assert!(table[3] as usize >= IWI_HEADER_LEN);
                // Content is the kept prefix (smallest levels) of the original levels.
                assert_eq!(&out[IWI_HEADER_LEN..], &big[IWI_HEADER_LEN..out.len()]);
            }
            let mut out = Vec::new();
            zip.by_name("images/small.iwi")
                .unwrap()
                .read_to_end(&mut out)
                .unwrap();
            assert_eq!(out, other);
            let mut out = Vec::new();
            zip.by_name("sound/x.wav")
                .unwrap()
                .read_to_end(&mut out)
                .unwrap();
            assert_eq!(out, b"hello");
        }
    }

    #[test]
    fn skips_unfit_images() {
        let mut nomips = iwi(32, 11);
        nomips[4] = 2;
        assert!(matches!(trim_iwi(&nomips, 8), Trim::NoMips));
        let mut sky = iwi(32, 11);
        sky[6] = 1;
        assert!(matches!(trim_iwi(&sky, 8), Trim::Special));
        let mut wavelet = iwi(32, 11);
        wavelet[8] = 6;
        assert!(matches!(trim_iwi(&wavelet, 8), Trim::Special));
        assert!(matches!(trim_iwi(&iwi(32, 11)[..100], 8), Trim::Unparsed));
        assert!(matches!(trim_iwi(b"junk", 8), Trim::Unparsed));
    }

    #[test]
    fn range_math() {
        assert_eq!(
            subtract(&[(0, 10), (20, 30)], &[(5, 22)]),
            [(0, 5), (22, 30)]
        );
        let mut buf = [0u8; 4];
        apply(&[(3, vec![1, 2, 3])], 2, &mut buf);
        assert_eq!(buf, [0, 1, 2, 3]);
    }
}
