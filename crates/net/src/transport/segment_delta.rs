use std::sync::Arc;

use crate::transport::frame::{FRAME_SEGMENTS, FrameParts, FrameSegments, WireMeta};
use crate::transport::meta_wire::META_SEGMENTS;

const SAME: u8 = 0;
const PATCH: u8 = 1;
const RAW: u8 = 2;
const PREFIXED: u8 = 3;

const RUN_MERGE_GAP: usize = 8;
const PREFIX_MIN_BYTES: usize = 4096;
#[cfg_attr(not(all(online, not(target_arch = "wasm32"))), allow(dead_code))]
const OUTER_LEVEL: i32 = 3;

pub const MAX_RECONSTRUCTED_FRAME_BYTES: usize = 256 * 1024;
pub const MAX_DELTA_INSTRUCTION_BYTES: usize = 256 * 1024;
#[cfg_attr(not(all(online, not(target_arch = "wasm32"))), allow(dead_code))]
const MAX_ZSTD_WINDOW_LOG: u32 = 18;

fn reserve_output(out: &mut Vec<u8>, additional: usize) -> Option<()> {
    let end = out.len().checked_add(additional)?;
    if end > MAX_RECONSTRUCTED_FRAME_BYTES {
        return None;
    }
    out.try_reserve_exact(additional).ok()
}

fn segment_slices<'a>(bytes: &'a [u8], lens: &FrameSegments) -> Option<[&'a [u8]; FRAME_SEGMENTS]> {
    let mut out = [&bytes[..0]; FRAME_SEGMENTS];
    let mut at = 0usize;
    for (slot, len) in out.iter_mut().zip(lens) {
        let end = at.checked_add(*len as usize)?;
        *slot = bytes.get(at..end)?;
        at = end;
    }
    (at == bytes.len()).then_some(out)
}

#[derive(Default)]
pub struct MetaPatchCache {
    entries: Vec<(Arc<WireMeta>, Arc<WireMeta>, Vec<u8>)>,
}

pub fn encode(new: &FrameParts, old: &FrameParts, cache: &mut MetaPatchCache) -> Vec<u8> {
    let new_segments = new.segments();
    let old_segments = old.segments();
    let mut out = Vec::with_capacity(4096);
    put_segment(&mut out, new_segments[0], old_segments[0]);
    let cached = cache
        .entries
        .iter()
        .find(|(n, o, _)| Arc::ptr_eq(n, &new.meta) && Arc::ptr_eq(o, &old.meta));
    match cached {
        Some((_, _, patch)) => out.extend_from_slice(patch),
        None => {
            let start = out.len();
            for index in 1..META_SEGMENTS {
                put_segment(&mut out, new_segments[index], old_segments[index]);
            }
            cache.entries.push((
                Arc::clone(&new.meta),
                Arc::clone(&old.meta),
                out[start..].to_vec(),
            ));
        }
    }
    for index in META_SEGMENTS..FRAME_SEGMENTS {
        put_segment(&mut out, new_segments[index], old_segments[index]);
    }
    out
}

fn put_segment(out: &mut Vec<u8>, new: &[u8], old: &[u8]) {
    if new == old {
        out.push(SAME);
        return;
    }
    if new.len() == old.len() {
        let mark = out.len();
        out.push(PATCH);
        put_patch(out, new, old);
        if out.len() - mark <= new.len() / 4 || new.len() < PREFIX_MIN_BYTES {
            return;
        }
        out.truncate(mark);
    }
    if new.len() >= PREFIX_MIN_BYTES
        && !old.is_empty()
        && let Ok(packed) = compress_against(new, old)
    {
        out.push(PREFIXED);
        put_varint(out, new.len());
        put_varint(out, packed.len());
        out.extend_from_slice(&packed);
        return;
    }
    out.push(RAW);
    put_varint(out, new.len());
    out.extend_from_slice(new);
}

pub fn decode(delta: &[u8], base: &[u8], base_lens: &FrameSegments) -> Option<Vec<u8>> {
    if delta.len() > MAX_DELTA_INSTRUCTION_BYTES || base.len() > MAX_RECONSTRUCTED_FRAME_BYTES {
        return None;
    }
    let old = segment_slices(base, base_lens)?;
    let mut input = delta;
    let mut out = Vec::new();
    for old in old {
        let (&tag, rest) = input.split_first()?;
        input = rest;
        match tag {
            SAME => {
                reserve_output(&mut out, old.len())?;
                out.extend_from_slice(old);
            }
            PATCH => {
                reserve_output(&mut out, old.len())?;
                let start = out.len();
                out.extend_from_slice(old);
                let runs = get_varint(&mut input)?;
                let mut at = 0usize;
                for _ in 0..runs {
                    at = at.checked_add(get_varint(&mut input)?)?;
                    let len = get_varint(&mut input)?;
                    let bytes = take(&mut input, len)?;
                    let end = at.checked_add(len)?;
                    out.get_mut(start + at..start + end)?.copy_from_slice(bytes);
                    at = end;
                }
            }
            RAW => {
                let len = get_varint(&mut input)?;
                reserve_output(&mut out, len)?;
                out.extend_from_slice(take(&mut input, len)?);
            }
            PREFIXED => {
                let len = get_varint(&mut input)?;
                let packed_len = get_varint(&mut input)?;
                let packed = take(&mut input, packed_len)?;
                let start = out.len();
                reserve_output(&mut out, len)?;
                out.resize(start.checked_add(len)?, 0);
                if prefixed_decompress(old, packed, &mut out[start..])? != len {
                    return None;
                }
            }
            _ => return None,
        }
    }
    input.is_empty().then_some(out)
}

// zstd is native-only (online); the browser decodes with ruzstd, which has no
// prefix reference. A PREFIXED segment is decoded there by rewriting it as one
// plain frame whose first blocks are the baseline segment (see
// `prefixed_as_plain_frame`).
#[cfg(all(online, not(target_arch = "wasm32")))]
fn prefixed_decompress(old: &[u8], packed: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut dctx = zstd::zstd_safe::DCtx::create();
    dctx.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(
        MAX_ZSTD_WINDOW_LOG,
    ))
    .ok()?;
    dctx.ref_prefix(old).ok()?;
    dctx.decompress(out, packed).ok()
}

#[cfg(all(online, target_arch = "wasm32"))]
fn prefixed_decompress(old: &[u8], packed: &[u8], out: &mut [u8]) -> Option<usize> {
    ruzstd_prefixed_decompress(old, packed, out)
}

#[cfg(not(online))]
fn prefixed_decompress(_old: &[u8], _packed: &[u8], _out: &mut [u8]) -> Option<usize> {
    None
}

#[cfg(all(online, any(target_arch = "wasm32", test)))]
const PREFIXED_WINDOW_LOG_MAX: u32 = 20;

/// Decodes a PREFIXED segment without prefix support: decode the rewritten
/// frame, which yields `old` followed by the new segment, and keep the tail.
#[cfg(all(online, any(target_arch = "wasm32", test)))]
fn ruzstd_prefixed_decompress(old: &[u8], packed: &[u8], out: &mut [u8]) -> Option<usize> {
    let frame = prefixed_as_plain_frame(old, packed, old.len().checked_add(out.len())?)?;
    let mut decoder = ruzstd::decoding::FrameDecoder::new();
    decoder.set_max_window_size(1 << PREFIXED_WINDOW_LOG_MAX);
    let mut all = Vec::new();
    all.try_reserve_exact(old.len() + out.len()).ok()?;
    decoder.decode_all_to_vec(&frame, &mut all).ok()?;
    let new = all.get(old.len()..)?;
    if all[..old.len()] != *old || new.len() != out.len() {
        return None;
    }
    out.copy_from_slice(new);
    Some(new.len())
}

/// Rewrites `packed`, one zstd frame compressed with `old` as its prefix
/// (`ref_prefix`), into one frame without a prefix: a header whose window
/// covers `total` bytes, `old` as raw blocks, then `packed`'s blocks.
///
/// This decodes the same as the prefixed frame (RFC 8878): a prefix is raw
/// content before the frame, both start with repeat offsets 1, 4, 8 and no
/// entropy tables, and raw blocks change neither. The checksum is dropped,
/// since it would cover only the new bytes.
#[cfg(all(online, any(target_arch = "wasm32", test)))]
fn prefixed_as_plain_frame(old: &[u8], packed: &[u8], total: usize) -> Option<Vec<u8>> {
    const MAGIC: [u8; 4] = 0xFD2F_B528u32.to_le_bytes();
    const MAX_BLOCK: usize = 128 * 1024;
    const LAST_BLOCK: u32 = 1;
    const RESERVED_BLOCK: u32 = 3;

    // Frame header: magic, descriptor, optional window, dictionary id, size.
    let mut input = packed;
    if take(&mut input, 4)? != MAGIC {
        return None;
    }
    let descriptor = *take(&mut input, 1)?.first()?;
    if descriptor & 0x08 != 0 {
        return None;
    }
    let single_segment = descriptor & 0x20 != 0;
    let checksum = descriptor & 0x04 != 0;
    let dict_id_bytes = [0, 1, 2, 4][usize::from(descriptor & 0x03)];
    let content_size_bytes = match descriptor >> 6 {
        0 => usize::from(single_segment),
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let skip = usize::from(!single_segment) + dict_id_bytes + content_size_bytes;
    take(&mut input, skip)?;

    // Blocks up to the last one; then only the optional checksum may remain.
    let blocks_start = packed.len() - input.len();
    loop {
        let header = take(&mut input, 3)?;
        let header = u32::from(header[0]) | u32::from(header[1]) << 8 | u32::from(header[2]) << 16;
        let kind = (header >> 1) & 3;
        let size = (header >> 3) as usize;
        let body = match kind {
            1 => 1, // RLE: one byte repeated `size` times
            RESERVED_BLOCK => return None,
            _ => size,
        };
        take(&mut input, body)?;
        if header & LAST_BLOCK != 0 {
            break;
        }
    }
    let blocks_end = packed.len() - input.len();
    if input.len() != if checksum { 4 } else { 0 } {
        return None;
    }

    let window_log = usize::BITS - total.max(1).saturating_sub(1).leading_zeros();
    let window_log = window_log.max(10);
    if window_log > PREFIXED_WINDOW_LOG_MAX {
        return None;
    }
    let raw_headers = old.len().div_ceil(MAX_BLOCK) * 3;
    let mut frame = Vec::new();
    frame
        .try_reserve_exact(6 + raw_headers + old.len() + blocks_end - blocks_start)
        .ok()?;
    frame.extend_from_slice(&MAGIC);
    // No content size, not single segment, no checksum, no dictionary.
    frame.push(0);
    frame.push(((window_log - 10) << 3) as u8);
    for chunk in old.chunks(MAX_BLOCK) {
        // Raw block (type 0), never last.
        let header = (chunk.len() as u32) << 3;
        frame.extend_from_slice(&header.to_le_bytes()[..3]);
        frame.extend_from_slice(chunk);
    }
    frame.extend_from_slice(&packed[blocks_start..blocks_end]);
    Some(frame)
}

#[cfg(all(online, not(target_arch = "wasm32")))]
pub fn compress(delta: &[u8]) -> std::io::Result<Vec<u8>> {
    if delta.len() > MAX_DELTA_INSTRUCTION_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "snapshot instruction budget exceeded",
        ));
    }
    thread_local! {
        static COMPRESSOR: std::cell::RefCell<Option<zstd::bulk::Compressor<'static>>> =
            const { std::cell::RefCell::new(None) };
    }
    COMPRESSOR.with_borrow_mut(|slot| {
        if slot.is_none() {
            *slot = Some(zstd::bulk::Compressor::new(OUTER_LEVEL)?);
        }
        slot.as_mut()
            .expect("compressor initialised")
            .compress(delta)
    })
}

#[cfg(not(all(online, not(target_arch = "wasm32"))))]
pub fn compress(_delta: &[u8]) -> std::io::Result<Vec<u8>> {
    Err(std::io::Error::other("snapshot compression needs the native online build"))
}

#[cfg(all(online, target_arch = "wasm32"))]
pub fn decompress(packed: &[u8], len: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    if len > MAX_DELTA_INSTRUCTION_BYTES
        || packed.len() > crate::transport::protocol::MAX_PACKET_BYTES as usize
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "snapshot decode budget exceeded",
        ));
    }
    let decoder = ruzstd::decoding::StreamingDecoder::new(packed).map_err(std::io::Error::other)?;
    let mut out = Vec::with_capacity(len);
    decoder.take(len as u64 + 1).read_to_end(&mut out)?;
    Ok(out)
}

#[cfg(not(online))]
pub fn decompress(_packed: &[u8], _len: usize) -> std::io::Result<Vec<u8>> {
    Err(std::io::Error::other("compressed snapshot needs the online build"))
}

#[cfg(all(online, not(target_arch = "wasm32")))]
pub fn decompress(packed: &[u8], len: usize) -> std::io::Result<Vec<u8>> {
    if len > MAX_DELTA_INSTRUCTION_BYTES
        || packed.len() > crate::transport::protocol::MAX_PACKET_BYTES as usize
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "snapshot decode budget exceeded",
        ));
    }
    thread_local! {
        static DECOMPRESSOR: std::cell::RefCell<Option<zstd::bulk::Decompressor<'static>>> =
            const { std::cell::RefCell::new(None) };
    }
    DECOMPRESSOR.with_borrow_mut(|slot| {
        if slot.is_none() {
            let mut decoder = zstd::bulk::Decompressor::new()?;
            decoder.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(
                MAX_ZSTD_WINDOW_LOG,
            ))?;
            *slot = Some(decoder);
        }
        slot.as_mut()
            .expect("decompressor initialised")
            .decompress(packed, len)
    })
}

fn first_difference(new: &[u8], old: &[u8], from: usize) -> Option<usize> {
    let mut at = from;
    while at + 8 <= new.len() {
        let a = u64::from_ne_bytes(new[at..at + 8].try_into().expect("8-byte window"));
        let b = u64::from_ne_bytes(old[at..at + 8].try_into().expect("8-byte window"));
        if a != b {
            break;
        }
        at += 8;
    }
    (at..new.len()).find(|&i| new[i] != old[i])
}

fn put_patch(out: &mut Vec<u8>, new: &[u8], old: &[u8]) {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut at = 0usize;
    while let Some(first) = first_difference(new, old, at) {
        let mut last = first;
        let mut scan = first;
        while scan < new.len() && scan - last <= RUN_MERGE_GAP {
            if new[scan] != old[scan] {
                last = scan;
            }
            scan += 1;
        }
        match runs.last_mut() {
            Some((_, end)) if first - *end <= RUN_MERGE_GAP => *end = last + 1,
            _ => runs.push((first, last + 1)),
        }
        at = last + 1;
    }
    put_varint(out, runs.len());
    let mut cursor = 0usize;
    for (start, end) in runs {
        put_varint(out, start - cursor);
        put_varint(out, end - start);
        out.extend_from_slice(&new[start..end]);
        cursor = end;
    }
}

#[cfg(not(all(online, not(target_arch = "wasm32"))))]
fn compress_against(_payload: &[u8], _baseline: &[u8]) -> Result<Vec<u8>, ()> {
    Err(())
}

#[cfg(all(online, not(target_arch = "wasm32")))]
fn compress_against(
    payload: &[u8],
    baseline: &[u8],
) -> Result<Vec<u8>, zstd::zstd_safe::ErrorCode> {
    let mut cctx = zstd::zstd_safe::CCtx::create();
    cctx.set_parameter(zstd::zstd_safe::CParameter::CompressionLevel(3))?;
    cctx.ref_prefix(baseline)?;
    let mut out = vec![0u8; zstd::zstd_safe::compress_bound(payload.len())];
    let len = cctx.compress2(&mut out[..], payload)?;
    out.truncate(len);
    Ok(out)
}

fn put_varint(out: &mut Vec<u8>, mut value: usize) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn get_varint(input: &mut &[u8]) -> Option<usize> {
    let mut value = 0usize;
    for shift in (0..35).step_by(7) {
        let (&byte, rest) = input.split_first()?;
        *input = rest;
        value |= usize::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn take<'a>(input: &mut &'a [u8], len: usize) -> Option<&'a [u8]> {
    if len > input.len() {
        return None;
    }
    let (head, rest) = input.split_at(len);
    *input = rest;
    Some(head)
}

#[cfg(all(test, online, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    /// Snapshot-like bytes: mostly repeated records with a few counters.
    fn segment(len: usize, seed: u32) -> Vec<u8> {
        let mut state = seed.wrapping_mul(2_654_435_761).max(1);
        (0..len)
            .map(|i| {
                if i % 64 < 8 {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    state as u8
                } else {
                    (i % 64) as u8
                }
            })
            .collect()
    }

    fn edited(old: &[u8], new_len: usize, seed: u32) -> Vec<u8> {
        let mut new = segment(new_len, seed);
        let keep = old.len().min(new_len);
        for at in (0..keep).filter(|at| at % 512 >= 16) {
            new[at] = old[at];
        }
        new
    }

    fn check(old_len: usize, new_len: usize) {
        let old = segment(old_len, 1);
        let new = edited(&old, new_len, 2);
        let packed = compress_against(&new, &old).expect("zstd prefix compress");
        assert!(
            packed.len() < new.len() / 2,
            "prefix not used: {}",
            packed.len()
        );
        let mut native = vec![0u8; new.len()];
        assert_eq!(
            prefixed_decompress(&old, &packed, &mut native),
            Some(new.len())
        );
        assert_eq!(native, new);
        let mut browser = vec![0u8; new.len()];
        assert_eq!(
            ruzstd_prefixed_decompress(&old, &packed, &mut browser),
            Some(new.len())
        );
        assert_eq!(browser, new);
    }

    #[test]
    fn ruzstd_decodes_prefixed_same_length() {
        check(20_000, 20_000);
    }

    #[test]
    fn ruzstd_decodes_prefixed_grown_and_shrunk() {
        check(9_000, 13_000);
        check(13_000, 4_096);
    }

    #[test]
    fn ruzstd_decodes_prefixed_over_one_raw_block() {
        check(200_000, 190_000);
    }

    #[test]
    fn ruzstd_rejects_bad_frames() {
        let old = segment(30_000, 3);
        let new = edited(&old, 30_000, 4);
        let packed = compress_against(&new, &old).expect("zstd prefix compress");
        let mut out = vec![0u8; new.len()];
        let mut bad_magic = packed.clone();
        bad_magic[0] ^= 1;
        assert_eq!(ruzstd_prefixed_decompress(&old, &bad_magic, &mut out), None);
        let truncated = &packed[..packed.len() - 1];
        assert_eq!(ruzstd_prefixed_decompress(&old, truncated, &mut out), None);
        let mut short = vec![0u8; new.len() - 1];
        assert_eq!(ruzstd_prefixed_decompress(&old, &packed, &mut short), None);
    }
}
