use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use web_time::Instant;

use asset_transport::{cache_flight, cache_get, cache_put, fnv1a64, fnv1a64_more};

pub const T5_WMA: i32 = 7;

const XWMA_CACHE_FORMAT: u32 = 2;
const XWMA_CACHE_KIND: &str = "xwma_pcm";
const XWMA_CACHE_MAGIC: &[u8; 8] = b"IWLXWMA\n";
const XWMA_CACHE_HEADER: usize = 8 + 4 * 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum XwmaDecodeError {
    Decode(String),
    EmptyPcm,
}

impl fmt::Display for XwmaDecodeError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(error) => write!(output, "T5 WMA2: {error}"),
            Self::EmptyPcm => output.write_str("T5 WMA2: empty PCM"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct XwmaClip<'a> {
    pub packets: &'a [u8],
    pub seek_table: &'a [u32],
    pub channels: u32,
    pub rate: u32,
}

pub fn decode_t5_xwma(
    packets: &[u8],
    seek_table: &[u32],
    channels: u32,
    rate: u32,
) -> Result<Vec<u8>, XwmaDecodeError> {
    let clip = XwmaClip {
        packets,
        seek_table,
        channels,
        rate,
    };
    let mut results = decode_t5_xwma_batch(std::slice::from_ref(&clip));
    debug_assert_eq!(results.len(), 1, "one clip in, one result out");
    results.pop().unwrap_or(Err(XwmaDecodeError::EmptyPcm))
}

pub fn decode_t5_xwma_batch(clips: &[XwmaClip<'_>]) -> Vec<Result<Vec<u8>, XwmaDecodeError>> {
    let key_at = Instant::now();
    let keys: Vec<String> = clips
        .iter()
        .map(|clip| cache_key(clip.packets, clip.seek_table, clip.channels, clip.rate))
        .collect();
    KEY_NS.fetch_add(key_at.elapsed().as_nanos() as u64, Ordering::Relaxed);

    let mut out: Vec<Option<Result<Vec<u8>, XwmaDecodeError>>> = vec![None; clips.len()];
    let mut pending = Vec::new();
    for (i, clip) in clips.iter().enumerate() {
        match cached(&keys[i], clip.channels, clip.rate) {
            Some(pcm) => out[i] = Some(Ok(pcm)),
            None => pending.push(i),
        }
    }

    pending.sort_unstable_by(|a, b| keys[*a].cmp(&keys[*b]));
    let mut flights = Vec::new();
    let mut leaders = Vec::new();
    let mut followers = Vec::new();
    let mut leader_of: HashMap<&str, usize> = HashMap::new();
    for &i in &pending {
        match leader_of.get(keys[i].as_str()) {
            Some(&leader) => followers.push((i, leader)),
            None => {
                leader_of.insert(keys[i].as_str(), i);
                flights.push(cache_flight(XWMA_CACHE_KIND, &keys[i]));
                leaders.push(i);
            }
        }
    }

    for &i in &leaders {
        if let Some(pcm) = cached(&keys[i], clips[i].channels, clips[i].rate) {
            out[i] = Some(Ok(pcm));
            continue;
        }
        let clip = clips[i];
        let decode_at = Instant::now();
        NATIVE.fetch_add(1, Ordering::Relaxed);
        let decoded =
            crate::wma_t5::decode(clip.packets, clip.seek_table, clip.channels, clip.rate)
                .map_err(|error| XwmaDecodeError::Decode(error.to_string()));
        DECODE_NS.fetch_add(decode_at.elapsed().as_nanos() as u64, Ordering::Relaxed);
        if let Ok(pcm) = &decoded {
            MISS.fetch_add(1, Ordering::Relaxed);
            PCM_BYTES.fetch_add(pcm.len() as u64, Ordering::Relaxed);
            store(&keys[i], clip.channels, clip.rate, pcm);
        }
        out[i] = Some(decoded);
    }

    for (i, leader) in followers {
        let answer = match &out[leader] {
            Some(Ok(pcm)) => {
                HIT.fetch_add(1, Ordering::Relaxed);
                PCM_BYTES.fetch_add(pcm.len() as u64, Ordering::Relaxed);
                Ok(pcm.clone())
            }
            Some(Err(error)) => Err(error.clone()),
            None => Err(XwmaDecodeError::EmptyPcm),
        };
        out[i] = Some(answer);
    }

    out.into_iter()
        .map(|answer| {
            let answer = answer.unwrap_or(Err(XwmaDecodeError::EmptyPcm));
            if answer.is_err() {
                FAILED.fetch_add(1, Ordering::Relaxed);
            }
            answer
        })
        .collect()
}

fn cache_encode(channels: u32, rate: u32, pcm: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(XWMA_CACHE_HEADER + pcm.len());
    out.extend_from_slice(XWMA_CACHE_MAGIC);
    out.extend_from_slice(&XWMA_CACHE_FORMAT.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

fn cache_decode(bytes: &[u8], channels: u32, rate: u32) -> Option<Vec<u8>> {
    if bytes.len() < XWMA_CACHE_HEADER || &bytes[..8] != XWMA_CACHE_MAGIC {
        return None;
    }
    let word = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    };
    if word(8)? != XWMA_CACHE_FORMAT || word(12)? != channels || word(16)? != rate {
        return None;
    }
    let len = word(20)? as usize;
    let pcm = bytes.get(XWMA_CACHE_HEADER..XWMA_CACHE_HEADER + len)?;
    (!pcm.is_empty()).then(|| pcm.to_vec())
}

fn cached(key: &str, channels: u32, rate: u32) -> Option<Vec<u8>> {
    let io_at = Instant::now();
    let bytes = cache_get(XWMA_CACHE_KIND, key);
    IO_NS.fetch_add(io_at.elapsed().as_nanos() as u64, Ordering::Relaxed);
    let pcm = cache_decode(&bytes?, channels, rate)?;
    HIT.fetch_add(1, Ordering::Relaxed);
    PCM_BYTES.fetch_add(pcm.len() as u64, Ordering::Relaxed);
    Some(pcm)
}

fn store(key: &str, channels: u32, rate: u32, pcm: &[u8]) {
    let io_at = Instant::now();
    if let Err(error) = cache_put(XWMA_CACHE_KIND, key, &cache_encode(channels, rate, pcm)) {
        diag::warn!(Audio, "xwma cache store {key}: {error}");
    }
    IO_NS.fetch_add(io_at.elapsed().as_nanos() as u64, Ordering::Relaxed);
}

fn cache_key(packets: &[u8], seek_table: &[u32], channels: u32, rate: u32) -> String {
    let mut hash = fnv1a64(&XWMA_CACHE_FORMAT.to_le_bytes());
    hash = fnv1a64_more(hash, &T5_WMA.to_le_bytes());
    hash = fnv1a64_more(hash, &channels.to_le_bytes());
    hash = fnv1a64_more(hash, &rate.to_le_bytes());
    hash = fnv1a64_more(hash, &(seek_table.len() as u64).to_le_bytes());
    for entry in seek_table {
        hash = fnv1a64_more(hash, &entry.to_le_bytes());
    }
    hash = fnv1a64_more(hash, &(packets.len() as u64).to_le_bytes());
    hash = fnv1a64_more(hash, packets);
    format!("{XWMA_CACHE_FORMAT:08x}-{hash:016x}")
}

static HIT: AtomicU64 = AtomicU64::new(0);
static MISS: AtomicU64 = AtomicU64::new(0);
static NATIVE: AtomicU64 = AtomicU64::new(0);
static FAILED: AtomicU64 = AtomicU64::new(0);
static PCM_BYTES: AtomicU64 = AtomicU64::new(0);
static DECODE_NS: AtomicU64 = AtomicU64::new(0);
static KEY_NS: AtomicU64 = AtomicU64::new(0);
static IO_NS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct XwmaDecodeCost {
    pub hit: u64,
    pub miss: u64,
    pub native: u64,
    pub failed: u64,
    pub pcm_bytes: u64,
    pub decode_ms: f64,
    pub key_ms: f64,
    pub io_ms: f64,
}

pub fn xwma_decode_cost() -> XwmaDecodeCost {
    XwmaDecodeCost {
        hit: HIT.load(Ordering::Relaxed),
        miss: MISS.load(Ordering::Relaxed),
        native: NATIVE.load(Ordering::Relaxed),
        failed: FAILED.load(Ordering::Relaxed),
        pcm_bytes: PCM_BYTES.load(Ordering::Relaxed),
        decode_ms: DECODE_NS.load(Ordering::Relaxed) as f64 / 1.0e6,
        key_ms: KEY_NS.load(Ordering::Relaxed) as f64 / 1.0e6,
        io_ms: IO_NS.load(Ordering::Relaxed) as f64 / 1.0e6,
    }
}
