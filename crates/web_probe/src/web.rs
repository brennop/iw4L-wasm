use wasm_bindgen::prelude::*;

#[wasm_bindgen(start)]
fn start() {
    std::panic::set_hook(Box::new(|info| {
        web_sys_log(&format!("panic: {info}"));
    }));
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn web_sys_log(message: &str);
}

#[wasm_bindgen]
pub fn install_pack(bytes: js_sys::Uint8Array) -> Result<(), JsValue> {
    gamefs::web::install_pack(bytes)
        .map_err(|error| JsValue::from_str(&format!("open pack: {error}")))
}

#[wasm_bindgen]
pub async fn probe(games_root: String, zone: String) -> Result<String, JsValue> {
    crate::probe(&games_root, &zone)
        .await
        .map(|report| report.render())
        .map_err(|error| JsValue::from_str(&error))
}

/// Size of the wasm linear memory; it never shrinks, so after a load it is the peak.
#[wasm_bindgen]
pub fn memory_bytes() -> f64 {
    (core::arch::wasm32::memory_size(0) * 65536) as f64
}

/// Live and peak heap bytes, to tell real use from allocator growth.
struct Counting;

static LIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static PEAK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        use std::sync::atomic::Ordering::Relaxed;
        let live = LIVE.fetch_add(layout.size(), Relaxed) + layout.size();
        PEAK.fetch_max(live, Relaxed);
        unsafe { std::alloc::System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        LIVE.fetch_sub(layout.size(), std::sync::atomic::Ordering::Relaxed);
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new: usize) -> *mut u8 {
        use std::sync::atomic::Ordering::Relaxed;
        if new >= layout.size() {
            let live = LIVE.fetch_add(new - layout.size(), Relaxed) + new - layout.size();
            PEAK.fetch_max(live, Relaxed);
        } else {
            LIVE.fetch_sub(layout.size() - new, Relaxed);
        }
        unsafe { std::alloc::System.realloc(ptr, layout, new) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Peak live heap bytes since start.
#[wasm_bindgen]
pub fn peak_heap_bytes() -> f64 {
    PEAK.load(std::sync::atomic::Ordering::Relaxed) as f64
}
