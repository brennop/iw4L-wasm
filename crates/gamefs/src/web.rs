//! The browser pack source: the pack stays in a JS buffer outside wasm memory,
//! and reads copy ranges in.

use std::io;
use std::sync::Arc;

struct JsSource {
    bytes: js_sys::Uint8Array,
}

// The wasm build has one thread, so the buffer is never shared.
unsafe impl Send for JsSource {}
unsafe impl Sync for JsSource {}

impl crate::pack::Source for JsSource {
    fn size(&self) -> u64 {
        u64::from(self.bytes.length())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let start = u32::try_from(offset).map_err(|_| io::Error::other("pack offset > 4 GiB"))?;
        let end = u32::try_from(buf.len())
            .ok()
            .and_then(|len| start.checked_add(len))
            .filter(|end| *end <= self.bytes.length())
            .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
        self.bytes.subarray(start, end).copy_to(buf);
        Ok(())
    }
}

/// Serves game files from the pack in `bytes`, mounted at `root`, from now on.
pub fn install_pack(bytes: js_sys::Uint8Array, root: &std::path::Path) -> io::Result<()> {
    let pack = crate::pack::Pack::open(Arc::new(JsSource { bytes }), root)?;
    crate::install(Arc::new(pack));
    Ok(())
}
