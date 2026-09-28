//! A pack is a sparse copy of the game files: for every file the loader read,
//! only the byte ranges it read. Layout:
//!
//! ```text
//! "IW4LPCK1"  chunk bytes ...  index  u64(index offset)
//! index = u32 file count, then per file:
//!   u16 path len, path, u64 file len, u32 chunk count,
//!   per chunk: u64 file offset, u64 pack offset, u64 len
//! ```
//!
//! Chunks are sorted and non-overlapping. Reading a range outside them is an
//! error naming the file and offset, which is how a missing range shows up.

use std::collections::BTreeMap;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{Backend, DirEntry, Meta, ReadSeek};

pub const MAGIC: &[u8; 8] = b"IW4LPCK1";

/// Random access to the pack bytes: a file natively, a JS buffer in the browser.
pub trait Source: Send + Sync {
    fn len(&self) -> u64;
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
}

impl Source for Vec<u8> {
    fn len(&self) -> u64 {
        self.as_slice().len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let start = offset as usize;
        let end = start + buf.len();
        buf.copy_from_slice(
            self.get(start..end)
                .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?,
        );
        Ok(())
    }
}

pub struct FileSource {
    file: std::sync::Mutex<std::fs::File>,
    len: u64,
}

impl FileSource {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            file: std::sync::Mutex::new(file),
            len,
        })
    }
}

impl Source for FileSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(buf)
    }
}

#[derive(Debug, Clone, Copy)]
struct Chunk {
    at: u64,
    pack_at: u64,
    len: u64,
}

#[derive(Debug)]
struct Entry {
    path: PathBuf,
    len: u64,
    chunks: Vec<Chunk>,
}

#[derive(Debug)]
enum Node {
    Dir(BTreeMap<String, Node>),
    File(Arc<Entry>),
}

pub struct Pack {
    source: Arc<dyn Source>,
    root: Node,
}

fn bad(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

fn take<'a>(bytes: &mut &'a [u8], n: usize) -> io::Result<&'a [u8]> {
    if bytes.len() < n {
        return Err(bad("pack index truncated"));
    }
    let (head, tail) = bytes.split_at(n);
    *bytes = tail;
    Ok(head)
}

fn take_u64(bytes: &mut &[u8]) -> io::Result<u64> {
    Ok(u64::from_le_bytes(take(bytes, 8)?.try_into().unwrap()))
}

fn components(path: &Path) -> Vec<String> {
    path.components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect()
}

impl Pack {
    pub fn open(source: Arc<dyn Source>) -> io::Result<Self> {
        let total = source.len();
        if total < 16 {
            return Err(bad("pack too small"));
        }
        let mut head = [0u8; 8];
        source.read_at(0, &mut head)?;
        if &head != MAGIC {
            return Err(bad("not an IW4L pack"));
        }
        let mut tail = [0u8; 8];
        source.read_at(total - 8, &mut tail)?;
        let index_at = u64::from_le_bytes(tail);
        let mut index = vec![0u8; (total - 8 - index_at) as usize];
        source.read_at(index_at, &mut index)?;
        let mut cursor = index.as_slice();
        let count = u32::from_le_bytes(take(&mut cursor, 4)?.try_into().unwrap());
        let mut root = Node::Dir(BTreeMap::new());
        for _ in 0..count {
            let path_len = u16::from_le_bytes(take(&mut cursor, 2)?.try_into().unwrap());
            let path = PathBuf::from(
                std::str::from_utf8(take(&mut cursor, path_len as usize)?)
                    .map_err(|_| bad("pack path is not utf-8"))?,
            );
            let len = take_u64(&mut cursor)?;
            let chunk_count = u32::from_le_bytes(take(&mut cursor, 4)?.try_into().unwrap());
            let mut chunks = Vec::with_capacity(chunk_count as usize);
            for _ in 0..chunk_count {
                chunks.push(Chunk {
                    at: take_u64(&mut cursor)?,
                    pack_at: take_u64(&mut cursor)?,
                    len: take_u64(&mut cursor)?,
                });
            }
            let entry = Arc::new(Entry {
                path: path.clone(),
                len,
                chunks,
            });
            let mut node = &mut root;
            let parts = components(&path);
            for (i, part) in parts.iter().enumerate() {
                let Node::Dir(children) = node else {
                    return Err(bad("pack path crosses a file"));
                };
                node = children.entry(part.clone()).or_insert_with(|| {
                    if i + 1 == parts.len() {
                        Node::File(Arc::clone(&entry))
                    } else {
                        Node::Dir(BTreeMap::new())
                    }
                });
            }
        }
        Ok(Self { source, root })
    }

    fn node(&self, path: &Path) -> Option<&Node> {
        let mut node = &self.root;
        for part in components(path) {
            let Node::Dir(children) = node else {
                return None;
            };
            node = children.get(&part)?;
        }
        Some(node)
    }

    fn entry(&self, path: &Path) -> io::Result<Arc<Entry>> {
        match self.node(path) {
            Some(Node::File(entry)) => Ok(Arc::clone(entry)),
            Some(Node::Dir(_)) => Err(io::Error::other(format!(
                "{} is a directory",
                path.display()
            ))),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} is not in the pack", path.display()),
            )),
        }
    }
}

fn meta_of(node: &Node) -> Meta {
    match node {
        Node::Dir(_) => Meta {
            is_dir: true,
            len: 0,
            modified: None,
        },
        Node::File(entry) => Meta {
            is_dir: false,
            len: entry.len,
            modified: None,
        },
    }
}

struct PackFile {
    source: Arc<dyn Source>,
    entry: Arc<Entry>,
    pos: u64,
}

impl Read for PackFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.entry.len || buf.is_empty() {
            return Ok(0);
        }
        let chunks = &self.entry.chunks;
        let at = chunks.partition_point(|chunk| chunk.at + chunk.len <= self.pos);
        let Some(chunk) = chunks.get(at).filter(|chunk| chunk.at <= self.pos) else {
            return Err(io::Error::other(format!(
                "pack has no bytes for {} at offset {}",
                self.entry.path.display(),
                self.pos
            )));
        };
        let within = self.pos - chunk.at;
        let n = (chunk.len - within).min(buf.len() as u64) as usize;
        self.source.read_at(chunk.pack_at + within, &mut buf[..n])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for PackFile {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let next = match to {
            SeekFrom::Start(at) => Some(at),
            SeekFrom::End(delta) => self.entry.len.checked_add_signed(delta),
            SeekFrom::Current(delta) => self.pos.checked_add_signed(delta),
        }
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        self.pos = next;
        Ok(next)
    }
}

impl Backend for Pack {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let entry = self.entry(path)?;
        let mut bytes = Vec::with_capacity(entry.len as usize);
        PackFile {
            source: Arc::clone(&self.source),
            entry,
            pos: 0,
        }
        .read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn ReadSeek>> {
        Ok(Box::new(PackFile {
            source: Arc::clone(&self.source),
            entry: self.entry(path)?,
            pos: 0,
        }))
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        match self.node(path) {
            Some(Node::Dir(children)) => Ok(children
                .iter()
                .map(|(name, node)| DirEntry {
                    path: path.join(name),
                    meta: meta_of(node),
                })
                .collect()),
            Some(Node::File(_)) => Err(io::Error::other("not a directory")),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} is not in the pack", path.display()),
            )),
        }
    }

    fn metadata(&self, path: &Path) -> io::Result<Meta> {
        self.node(path).map(meta_of).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} is not in the pack", path.display()),
            )
        })
    }
}

/// One file to store: its length and the sorted, merged ranges to copy.
pub struct WriteFile {
    pub path: PathBuf,
    pub len: u64,
    pub ranges: Vec<(u64, u64)>,
}

/// Writes a pack. `read(path, start, buf)` supplies the bytes from the real files.
pub fn write(
    out: &mut impl Write,
    files: &[WriteFile],
    mut read: impl FnMut(&Path, u64, &mut [u8]) -> io::Result<()>,
) -> io::Result<u64> {
    out.write_all(MAGIC)?;
    let mut at = MAGIC.len() as u64;
    let mut index = Vec::new();
    index.extend((files.len() as u32).to_le_bytes());
    let mut buf = Vec::new();
    for file in files {
        let path = file.path.to_string_lossy();
        index.extend((path.len() as u16).to_le_bytes());
        index.extend(path.as_bytes());
        index.extend(file.len.to_le_bytes());
        index.extend((file.ranges.len() as u32).to_le_bytes());
        for &(start, end) in &file.ranges {
            buf.resize((end - start) as usize, 0);
            read(&file.path, start, &mut buf)?;
            out.write_all(&buf)?;
            index.extend(start.to_le_bytes());
            index.extend(at.to_le_bytes());
            index.extend((end - start).to_le_bytes());
            at += end - start;
        }
    }
    out.write_all(&index)?;
    out.write_all(&at.to_le_bytes())?;
    Ok(at + index.len() as u64 + 8)
}
