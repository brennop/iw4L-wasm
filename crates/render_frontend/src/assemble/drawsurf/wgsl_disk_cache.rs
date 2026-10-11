use std::sync::atomic::{AtomicU64, Ordering};

use d3d9_sm3::{PassLoweringAbi, PassModules, PassWgsl};

use super::material_runtime::RuntimeShaderPair;

// Bump when emitted WGSL changes as well as when the binary format changes.
pub const WGSL_CACHE_FORMAT: u32 = 10;

const MAGIC: &[u8; 8] = b"IWLWGSL\n";

/// Magic, format, five counts and the module kind, before the first module text.
const HEADER_LEN: usize = 8 + 4 + 5 * 4 + 4;
const KIND_COMBINED: u32 = 0;
const KIND_SPLIT: u32 = 1;

static HIT: AtomicU64 = AtomicU64::new(0);
static MISS: AtomicU64 = AtomicU64::new(0);
static IO_NS: AtomicU64 = AtomicU64::new(0);

pub fn stats() -> (u64, u64, f64) {
    (
        HIT.load(Ordering::Relaxed),
        MISS.load(Ordering::Relaxed),
        IO_NS.load(Ordering::Relaxed) as f64 / 1.0e6,
    )
}

pub fn load(pair: RuntimeShaderPair, abi: &PassLoweringAbi) -> Option<PassWgsl> {
    let key = cache_key(pair, abi);
    let started = web_time::Instant::now();
    let bytes = assets::cache_get("wgsl", &key)?;
    IO_NS.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    let module = decode(&bytes)?;
    HIT.fetch_add(1, Ordering::Relaxed);
    Some(module)
}

pub fn store(pair: RuntimeShaderPair, abi: &PassLoweringAbi, module: &PassWgsl) {
    MISS.fetch_add(1, Ordering::Relaxed);
    let key = cache_key(pair, abi);
    let bytes = encode(module);
    let started = web_time::Instant::now();
    if let Err(error) = assets::cache_put("wgsl", &key, &bytes) {
        diag::warn!(World, "wgsl cache store {key}: {error}");
    }
    IO_NS.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
}

fn cache_key(pair: RuntimeShaderPair, abi: &PassLoweringAbi) -> String {
    let mut hash = assets::fnv1a64(&WGSL_CACHE_FORMAT.to_le_bytes());
    hash = assets::fnv1a64_more(hash, &pair.vertex.program_hash.to_le_bytes());
    hash = assets::fnv1a64_more(hash, &pair.pixel.program_hash.to_le_bytes());
    hash = assets::fnv1a64_more(hash, format!("{abi:?}").as_bytes());
    format!("{hash:016x}")
}

fn encode(module: &PassWgsl) -> Vec<u8> {
    let (kind, first, second) = match &module.modules {
        PassModules::Combined(source) => (KIND_COMBINED, source.as_bytes(), &[][..]),
        PassModules::Split { vertex, fragment } => {
            (KIND_SPLIT, vertex.as_bytes(), fragment.as_bytes())
        }
    };
    let mut out = Vec::with_capacity(HEADER_LEN + 12 + first.len() + second.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&WGSL_CACHE_FORMAT.to_le_bytes());
    out.extend_from_slice(&(module.attribute_count as u32).to_le_bytes());
    out.extend_from_slice(&(module.varying_count as u32).to_le_bytes());
    out.extend_from_slice(&(module.vertex_constant_len as u32).to_le_bytes());
    out.extend_from_slice(&(module.pixel_constant_len as u32).to_le_bytes());
    out.extend_from_slice(&(module.sampler_count as u32).to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&(first.len() as u32).to_le_bytes());
    out.extend_from_slice(first);
    if kind == KIND_SPLIT {
        out.extend_from_slice(&(second.len() as u32).to_le_bytes());
        out.extend_from_slice(second);
    }
    out
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn read_text(bytes: &[u8], at: usize) -> Option<(String, usize)> {
    let len = read_u32(bytes, at)? as usize;
    let start = at + 4;
    let text = std::str::from_utf8(bytes.get(start..start + len)?).ok()?;
    Some((text.to_owned(), start + len))
}

fn decode(bytes: &[u8]) -> Option<PassWgsl> {
    if bytes.len() < HEADER_LEN {
        return None;
    }
    if &bytes[..8] != MAGIC {
        return None;
    }
    if read_u32(bytes, 8)? != WGSL_CACHE_FORMAT {
        return None;
    }
    let attribute_count = read_u32(bytes, 12)? as usize;
    let varying_count = read_u32(bytes, 16)? as usize;
    let vertex_constant_len = read_u32(bytes, 20)? as usize;
    let pixel_constant_len = read_u32(bytes, 24)? as usize;
    let sampler_count = read_u32(bytes, 28)? as usize;
    let kind = read_u32(bytes, 32)?;
    let (first, next) = read_text(bytes, 36)?;
    let modules = match kind {
        KIND_COMBINED => PassModules::Combined(first.into()),
        KIND_SPLIT => PassModules::Split {
            vertex: first.into(),
            fragment: read_text(bytes, next)?.0.into(),
        },
        _ => return None,
    };
    Some(PassWgsl {
        modules,
        attribute_count,
        varying_count,
        vertex_constant_len,
        pixel_constant_len,
        sampler_count,
    })
}
