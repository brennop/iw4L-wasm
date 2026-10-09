//! Browser entry: launch args come from the page URL, logs go to the browser
//! console, and a panic is shown on the page instead of ending a process.
//! Info and debug lines reach the console only with `?log=1` or after
//! `iw4l.log(true)`; warnings and errors always do, and the in-memory log file
//! keeps every line either way.
//!
//! Defaults to `mp_rust`; `?map=<zone>` overrides it. `?mode=` names the launch
//! word (`menu`, `map`, `play`, `export-gltf`) when it isn't implied. `cmds`,
//! `acceptance` and `games` map to `--cmds`, `--render-acceptance` and the games
//! root.
//!
//! Artifacts (log, acceptance ledger, captures, reports) are kept in memory;
//! the page reads them through `window.iw4l` (see `web/index.html`). Settings
//! and classes are also mirrored to `localStorage` so they survive a reload.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use wasm_bindgen::prelude::*;

/// Where the game files appear in the virtual file system unless `?games=` says otherwise.
const DEFAULT_GAMES_ROOT: &str = "/games";
const DEFAULT_MAP: &str = "mp_rust";
const ERROR_OVERLAY_ID: &str = "iw4l-error";
const ARTIFACTS_ROOT: &str = "iw4l-artifacts";
/// Artifacts that outlive the page, stored under `localStorage["iw4l:<path>"]`.
const PERSISTED: [&str; 5] = [
    "iw4l-artifacts/settings.cfg",
    "iw4l-artifacts/profile/classes.txt",
    "iw4l-artifacts/profile/barracks.txt",
    "iw4l-artifacts/profile.cfg",
    "iw4l-artifacts/account.dat",
];

/// localStorage holds text, so a binary artifact (the account file: signing-key
/// seed and player-data buffer) is stored as hex; everything else as-is.
fn is_binary(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "dat")
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if !text.is_ascii() || text.len() % 2 != 0 {
        return None;
    }
    (0..text.len() / 2)
        .map(|index| u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok())
        .collect()
}

static ARTIFACTS: OnceLock<Arc<artifactfs::Memory>> = OnceLock::new();

/// Whether info/debug lines go to the browser console. Off by default: the
/// engine logs per frame and per action, which floods the console.
static CONSOLE_INFO: AtomicBool = AtomicBool::new(false);

/// Turns info/debug lines in the browser console on or off (`iw4l.log(on)`).
#[wasm_bindgen]
pub fn set_console_log(on: bool) {
    CONSOLE_INFO.store(on, Ordering::Relaxed);
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn console_error(message: &str);
    #[wasm_bindgen(js_namespace = console, js_name = warn)]
    fn console_warn(message: &str);
    #[wasm_bindgen(js_namespace = console, js_name = log)]
    fn console_log(message: &str);
}

/// The page fetches the pack into a `Uint8Array` at `window.iw4l_pack` before
/// starting the app (`main` runs inside `init()`, so it can't be handed over by
/// a call). The bytes stay in the JS buffer; reads copy ranges in.
fn install_pack_from_page(root: &Path) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let value = js_sys::Reflect::get(&window, &"iw4l_pack".into()).unwrap_or(JsValue::UNDEFINED);
    let Ok(bytes) = value.dyn_into::<js_sys::Uint8Array>() else {
        diag::warn!(
            Launch,
            "no pack on the page (window.iw4l_pack); game files will be missing"
        );
        return;
    };
    match gamefs::web::install_pack(bytes.clone(), root) {
        Ok(()) => diag::info!(Launch, "pack installed: {} bytes", bytes.length()),
        Err(error) => diag::exit_launch_error(&format!("open pack: {error}")),
    }
}

pub fn main() {
    std::panic::set_hook(Box::new(|info| {
        let message = format!("panic: {info}");
        console_error(&message);
        show_error(&message);
    }));
    let query = UrlQuery::from_page();
    set_console_log(query.get("log").is_some_and(|value| value != "0"));
    if query.get("pred_log").is_some_and(|value| value != "0") {
        net::client::pred_log::enable();
    }
    diag::set_console(|level, line| match level {
        diag::Level::Error => console_error(line),
        diag::Level::Warn => console_warn(line),
        diag::Level::Info | diag::Level::Debug => {
            if CONSOLE_INFO.load(Ordering::Relaxed) {
                console_log(line);
            }
        }
    });

    let args = launch_args(&query);
    console::set_startup_args(args.clone());
    let games_root = PathBuf::from(query.get("games").unwrap_or(DEFAULT_GAMES_ROOT.into()));
    asset_transport::set_games_root_override(games_root.clone());

    install_artifact_sink();
    install_pack_from_page(&games_root);
    bootstrap::bench::arm();
    let artifacts = PathBuf::from(ARTIFACTS_ROOT);
    diag::init_log(&artifacts);
    diag::info!(Launch, "web launch args: {args:?}");
    let (mode, acceptance, cheats) =
        bootstrap::parse_cli(args.into_iter()).unwrap_or_else(|e| diag::exit_launch_error(&e));
    let games =
        asset_transport::games_root_from_env().unwrap_or_else(|e| diag::exit_launch_error(&e));
    bootstrap::launch(games, artifacts, mode, acceptance, cheats);
}

fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

fn storage_key(path: &Path) -> String {
    format!("iw4l:{}", path.display())
}

fn install_artifact_sink() {
    let memory = Arc::new(artifactfs::Memory::with_mirror(|path, bytes| {
        if !PERSISTED.iter().any(|kept| Path::new(kept) == path) {
            return;
        }
        let Some(storage) = local_storage() else {
            return;
        };
        let text = if is_binary(path) {
            hex_encode(bytes)
        } else {
            String::from_utf8_lossy(bytes).into_owned()
        };
        if let Err(error) = storage.set_item(&storage_key(path), &text) {
            console_warn(&format!(
                "localStorage: {} not saved: {error:?}",
                path.display()
            ));
        }
    }));
    if let Some(storage) = local_storage() {
        for kept in PERSISTED {
            if let Ok(Some(text)) = storage.get_item(&storage_key(Path::new(kept))) {
                let bytes = if is_binary(Path::new(kept)) {
                    hex_decode(&text)
                } else {
                    Some(text.into_bytes())
                };
                if let Some(bytes) = bytes {
                    memory.insert(Path::new(kept), bytes);
                }
            }
        }
    }
    artifactfs::install(Arc::clone(&memory) as Arc<dyn artifactfs::Backend>);
    let _ = ARTIFACTS.set(memory);
}

/// Every artifact this page has written, as `[{ path, bytes }]` in path order.
#[wasm_bindgen]
pub fn artifacts() -> js_sys::Array {
    let list = js_sys::Array::new();
    for (path, bytes) in ARTIFACTS
        .get()
        .map(|memory| memory.list())
        .unwrap_or_default()
    {
        let entry = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&entry, &"path".into(), &path.display().to_string().into());
        let _ = js_sys::Reflect::set(&entry, &"bytes".into(), &(bytes as f64).into());
        list.push(&entry);
    }
    list
}

/// One artifact's bytes, or `undefined` if there is none at `path`.
#[wasm_bindgen]
pub fn artifact(path: &str) -> Option<Vec<u8>> {
    ARTIFACTS.get()?.get(Path::new(path))
}

struct UrlQuery(Option<web_sys::UrlSearchParams>);

impl UrlQuery {
    fn from_page() -> Self {
        let search = web_sys::window().and_then(|window| window.location().search().ok());
        Self(search.and_then(|search| web_sys::UrlSearchParams::new_with_str(&search).ok()))
    }

    fn get(&self, name: &str) -> Option<String> {
        self.0.as_ref()?.get(name).filter(|value| !value.is_empty())
    }
}

/// The argv a native launch would get for the same run.
fn launch_args(query: &UrlQuery) -> Vec<String> {
    let mode = query.get("mode").unwrap_or_else(|| "map".into());
    // `?mode=menu` boots the frontend; every other mode takes a map.
    let map = (mode != "menu").then(|| query.get("map").unwrap_or_else(|| DEFAULT_MAP.into()));
    let mut args = vec![mode];
    args.extend(map);
    if let Some(cmds) = query.get("cmds") {
        args.extend(["--cmds".into(), cmds]);
    }
    if let Some(dir) = query.get("acceptance") {
        args.extend(["--render-acceptance".into(), dir]);
    }
    args
}

/// A full-page text panel over the canvas; later errors are appended to it.
fn show_error(message: &str) {
    let Some(document) = web_sys::window().and_then(|window| window.document()) else {
        return;
    };
    let overlay = match document.get_element_by_id(ERROR_OVERLAY_ID) {
        Some(overlay) => overlay,
        None => {
            let Ok(overlay) = document.create_element("pre") else {
                return;
            };
            overlay.set_id(ERROR_OVERLAY_ID);
            let _ = overlay.set_attribute(
                "style",
                "position:fixed;inset:0;margin:0;padding:24px;overflow:auto;z-index:2147483647;\
                 background:rgba(12,12,14,0.94);color:#f4f4f5;font:14px/1.5 ui-monospace,monospace;\
                 white-space:pre-wrap;",
            );
            let Some(body) = document.body() else {
                return;
            };
            if body.append_child(&overlay).is_err() {
                return;
            }
            overlay
        }
    };
    let text = match overlay.text_content() {
        Some(earlier) if !earlier.is_empty() => format!("{earlier}\n\n{message}"),
        _ => format!("iw4l stopped\n\n{message}"),
    };
    overlay.set_text_content(Some(&text));
}
