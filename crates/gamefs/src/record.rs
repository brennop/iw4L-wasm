//! `IW4L_FS_RECORD=<file>`: log every byte range the loader reads, one line per
//! read, so `xtask web-pack` can copy exactly those ranges into a pack.
//!
//! Lines are tab separated: `R path len start end`, `D path` (listed),
//! `M path` (stat).

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Mutex;

use crate::{Backend, DirEntry, Meta, Native, ReadSeek};

pub struct Recording {
    inner: Native,
    log: std::sync::Arc<Mutex<File>>,
}

impl Recording {
    pub fn create(log: &Path) -> io::Result<Self> {
        Ok(Self {
            inner: Native,
            log: std::sync::Arc::new(Mutex::new(File::create(log)?)),
        })
    }
}

fn line(log: &Mutex<File>, text: String) {
    let mut file = log
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _ = file.write_all(text.as_bytes());
}

struct RecFile {
    inner: Box<dyn ReadSeek>,
    path: String,
    len: u64,
    pos: u64,
    log: std::sync::Arc<Mutex<File>>,
}

impl Read for RecFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            line(
                &self.log,
                format!(
                    "R\t{}\t{}\t{}\t{}\n",
                    self.path,
                    self.len,
                    self.pos,
                    self.pos + n as u64
                ),
            );
            self.pos += n as u64;
        }
        Ok(n)
    }
}

impl Seek for RecFile {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        self.pos = self.inner.seek(to)?;
        Ok(self.pos)
    }
}

impl Backend for Recording {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let bytes = self.inner.read(path)?;
        line(
            &self.log,
            format!(
                "R\t{}\t{}\t0\t{}\n",
                path.display(),
                bytes.len(),
                bytes.len()
            ),
        );
        Ok(bytes)
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn ReadSeek>> {
        let len = self.inner.metadata(path)?.len;
        Ok(Box::new(RecFile {
            inner: self.inner.open(path)?,
            path: path.display().to_string(),
            len,
            pos: 0,
            log: std::sync::Arc::clone(&self.log),
        }))
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        line(&self.log, format!("D\t{}\n", path.display()));
        self.inner.read_dir(path)
    }

    fn metadata(&self, path: &Path) -> io::Result<Meta> {
        line(&self.log, format!("M\t{}\n", path.display()));
        self.inner.metadata(path)
    }

    /// Caches must stay cold while recording, or a hit would hide a read the
    /// browser (which has no cache) will need.
    fn is_native(&self) -> bool {
        false
    }
}
