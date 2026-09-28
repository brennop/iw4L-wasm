//! Every read of the player's game files goes through here, so a build without
//! a filesystem (the browser) can serve them from a pack. The default backend
//! is `std::fs`; [`install`] swaps it.

use std::io::{self, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use web_time::SystemTime;

pub trait ReadSeek: Read + Seek + Send {}
impl<T: Read + Seek + Send> ReadSeek for T {}

#[derive(Debug, Clone)]
pub struct Meta {
    pub is_dir: bool,
    pub len: u64,
    pub modified: Option<SystemTime>,
}

impl Meta {
    pub fn is_file(&self) -> bool {
        !self.is_dir
    }
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub path: PathBuf,
    pub meta: Meta,
}

pub trait Backend: Send + Sync {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    fn open(&self, path: &Path) -> io::Result<Box<dyn ReadSeek>>;
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>>;
    fn metadata(&self, path: &Path) -> io::Result<Meta>;
    /// True when files live on a real disk that other code may also write to.
    fn is_native(&self) -> bool {
        false
    }
}

struct Native;

fn meta_of(meta: &std::fs::Metadata) -> Meta {
    Meta {
        is_dir: meta.is_dir(),
        len: meta.len(),
        modified: meta.modified().ok(),
    }
}

impl Backend for Native {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn ReadSeek>> {
        Ok(Box::new(std::io::BufReader::new(std::fs::File::open(
            path,
        )?)))
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        Ok(std::fs::read_dir(path)?
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let meta = std::fs::metadata(entry.path()).ok()?;
                Some(DirEntry {
                    path: entry.path(),
                    meta: meta_of(&meta),
                })
            })
            .collect())
    }

    fn metadata(&self, path: &Path) -> io::Result<Meta> {
        std::fs::metadata(path).map(|meta| meta_of(&meta))
    }

    fn is_native(&self) -> bool {
        true
    }
}

fn slot() -> &'static RwLock<Arc<dyn Backend>> {
    static SLOT: std::sync::OnceLock<RwLock<Arc<dyn Backend>>> = std::sync::OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(Arc::new(Native)))
}

fn backend() -> Arc<dyn Backend> {
    Arc::clone(
        &slot()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

pub fn install(backend: Arc<dyn Backend>) {
    *slot()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = backend;
}

pub fn is_native() -> bool {
    backend().is_native()
}

pub fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    backend().read(path.as_ref())
}

pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    String::from_utf8(read(path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub fn open(path: impl AsRef<Path>) -> io::Result<Box<dyn ReadSeek>> {
    backend().open(path.as_ref())
}

pub fn read_dir(path: impl AsRef<Path>) -> io::Result<Vec<DirEntry>> {
    backend().read_dir(path.as_ref())
}

pub fn metadata(path: impl AsRef<Path>) -> io::Result<Meta> {
    backend().metadata(path.as_ref())
}

pub fn is_dir(path: impl AsRef<Path>) -> bool {
    metadata(path).is_ok_and(|meta| meta.is_dir)
}

pub fn is_file(path: impl AsRef<Path>) -> bool {
    metadata(path).is_ok_and(|meta| !meta.is_dir)
}

pub fn exists(path: impl AsRef<Path>) -> bool {
    metadata(path).is_ok()
}

/// Native paths are resolved on disk; virtual backends keep the path as given.
pub fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    if is_native() {
        std::fs::canonicalize(path)
    } else {
        Ok(path.to_path_buf())
    }
}
