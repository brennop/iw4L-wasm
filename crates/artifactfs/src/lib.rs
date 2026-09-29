//! Every file a run writes for itself — logs, traces, reports, acceptance
//! ledgers, captures, dumps, settings — goes through here, so a build without
//! a filesystem (the browser) can keep them in memory and hand them to the
//! page. The default backend is `std::fs`; [`install`] swaps it.
//!
//! Game files are read through `gamefs`, not this.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

pub trait Backend: Send + Sync {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// Create or replace the whole file.
    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    /// [`Backend::write`], and on disk wait until the bytes are stored.
    fn write_durable(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.write(path, bytes)
    }
    /// Write a file that must not exist yet; `AlreadyExists` otherwise. Readers
    /// never see it half-written.
    fn write_new(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    /// A writer that appends to the file, creating it if missing.
    fn append(&self, path: &Path) -> io::Result<Box<dyn Write + Send>>;
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    fn len(&self, path: &Path) -> io::Result<u64>;
    /// True when artifacts land on a real disk.
    fn is_native(&self) -> bool {
        false
    }
}

struct Native;

impl Backend for Native {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        std::fs::write(path, bytes)
    }

    fn write_durable(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let mut file = std::fs::File::create(path)?;
        file.write_all(bytes)?;
        file.sync_all()
    }

    /// Written under a temporary name, then hard-linked into place: the link
    /// fails if the target exists, and a reader never sees a partial file.
    fn write_new(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let directory = path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} has no parent", path.display()),
            )
        })?;
        let file_name = path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} has no file name", path.display()),
            )
        })?;
        let temporary = directory.join(format!(".{}.tmp", file_name.to_string_lossy()));
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(bytes)?;
            file.flush()?;
            drop(file);
            std::fs::hard_link(&temporary, path)?;
            std::fs::remove_file(&temporary)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }

    fn append(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        Ok(Box::new(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?,
        ))
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn len(&self, path: &Path) -> io::Result<u64> {
        std::fs::metadata(path).map(|meta| meta.len())
    }

    fn is_native(&self) -> bool {
        true
    }
}

type Files = Arc<Mutex<BTreeMap<PathBuf, Vec<u8>>>>;
type Mirror = Box<dyn Fn(&Path, &[u8]) + Send + Sync>;

/// Artifacts held in memory, keyed by path. Directories are implicit.
#[derive(Default)]
pub struct Memory {
    files: Files,
    mirror: Option<Mirror>,
}

impl Memory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Called with a file's full contents each time `write`, `write_new` or
    /// `rename` completes it (appends are not mirrored). The browser uses it to
    /// keep settings in `localStorage`.
    pub fn with_mirror(mirror: impl Fn(&Path, &[u8]) + Send + Sync + 'static) -> Self {
        Self {
            files: Files::default(),
            mirror: Some(Box::new(mirror)),
        }
    }

    /// Put a file in place without mirroring it (seeding from saved state).
    pub fn insert(&self, path: &Path, bytes: Vec<u8>) {
        self.lock().insert(key(path), bytes);
    }

    /// Every file and its size, in path order.
    pub fn list(&self) -> Vec<(PathBuf, u64)> {
        self.lock()
            .iter()
            .map(|(path, bytes)| (path.clone(), bytes.len() as u64))
            .collect()
    }

    pub fn get(&self, path: &Path) -> Option<Vec<u8>> {
        self.lock().get(&key(path)).cloned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<PathBuf, Vec<u8>>> {
        self.files.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn mirror(&self, path: &Path, bytes: &[u8]) {
        if let Some(mirror) = &self.mirror {
            mirror(path, bytes);
        }
    }
}

/// `a/./b` and `a//b` name the same file.
fn key(path: &Path) -> PathBuf {
    path.components().collect()
}

fn not_found(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("{}: no such artifact", path.display()),
    )
}

struct MemoryAppend {
    files: Files,
    path: PathBuf,
}

impl Write for MemoryAppend {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(self.path.clone())
            .or_default()
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Backend for Memory {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.get(path).ok_or_else(|| not_found(path))
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let path = key(path);
        self.lock().insert(path.clone(), bytes.to_vec());
        self.mirror(&path, bytes);
        Ok(())
    }

    fn write_new(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let path = key(path);
        {
            let mut files = self.lock();
            if files.contains_key(&path) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists", path.display()),
                ));
            }
            files.insert(path.clone(), bytes.to_vec());
        }
        self.mirror(&path, bytes);
        Ok(())
    }

    fn append(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        let path = key(path);
        self.lock().entry(path.clone()).or_default();
        Ok(Box::new(MemoryAppend {
            files: Arc::clone(&self.files),
            path,
        }))
    }

    fn create_dir_all(&self, _path: &Path) -> io::Result<()> {
        Ok(())
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let to = key(to);
        let bytes = {
            let mut files = self.lock();
            let bytes = files.remove(&key(from)).ok_or_else(|| not_found(from))?;
            files.insert(to.clone(), bytes.clone());
            bytes
        };
        self.mirror(&to, &bytes);
        Ok(())
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.lock()
            .remove(&key(path))
            .map(drop)
            .ok_or_else(|| not_found(path))
    }

    fn len(&self, path: &Path) -> io::Result<u64> {
        self.lock()
            .get(&key(path))
            .map(|bytes| bytes.len() as u64)
            .ok_or_else(|| not_found(path))
    }
}

fn slot() -> &'static RwLock<Arc<dyn Backend>> {
    static SLOT: std::sync::OnceLock<RwLock<Arc<dyn Backend>>> = std::sync::OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(Arc::new(Native)))
}

fn backend() -> Arc<dyn Backend> {
    Arc::clone(&slot().read().unwrap_or_else(PoisonError::into_inner))
}

pub fn install(backend: Arc<dyn Backend>) {
    *slot().write().unwrap_or_else(PoisonError::into_inner) = backend;
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

pub fn write(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) -> io::Result<()> {
    backend().write(path.as_ref(), bytes.as_ref())
}

pub fn write_durable(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) -> io::Result<()> {
    backend().write_durable(path.as_ref(), bytes.as_ref())
}

pub fn write_new(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) -> io::Result<()> {
    backend().write_new(path.as_ref(), bytes.as_ref())
}

pub fn append(path: impl AsRef<Path>) -> io::Result<Box<dyn Write + Send>> {
    backend().append(path.as_ref())
}

pub fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    backend().create_dir_all(path.as_ref())
}

pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    backend().rename(from.as_ref(), to.as_ref())
}

pub fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
    backend().remove_file(path.as_ref())
}

pub fn len(path: impl AsRef<Path>) -> io::Result<u64> {
    backend().len(path.as_ref())
}

/// Encode 8-bit RGB pixels as PNG and write them.
pub fn write_png_rgb8(
    path: impl AsRef<Path>,
    width: u32,
    height: u32,
    rgb: &[u8],
) -> io::Result<()> {
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(io::Error::other)?;
    writer.write_image_data(rgb).map_err(io::Error::other)?;
    writer.finish().map_err(io::Error::other)?;
    write(path, bytes)
}
