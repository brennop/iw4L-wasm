//! Browser entry: launch args come from the page URL, logs go to the browser
//! console, and a panic is shown on the page instead of ending a process.
//!
//! `?map=<zone>` loads a map, otherwise the menu; `?mode=` names the launch word
//! (`menu`, `map`, `play`, `export-gltf`) when it isn't implied. `cmds`,
//! `acceptance` and `games` map to `--cmds`, `--render-acceptance` and the games
//! root.

use std::path::PathBuf;

use wasm_bindgen::prelude::*;

/// Where the game files appear in the virtual file system unless `?games=` says otherwise.
const DEFAULT_GAMES_ROOT: &str = "/games";
const ERROR_OVERLAY_ID: &str = "iw4l-error";

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn console_error(message: &str);
    #[wasm_bindgen(js_namespace = console, js_name = warn)]
    fn console_warn(message: &str);
    #[wasm_bindgen(js_namespace = console, js_name = log)]
    fn console_log(message: &str);
}

pub fn main() {
    std::panic::set_hook(Box::new(|info| {
        let message = format!("panic: {info}");
        console_error(&message);
        show_error(&message);
    }));
    diag::set_console(|level, line| match level {
        diag::Level::Error => console_error(line),
        diag::Level::Warn => console_warn(line),
        diag::Level::Info | diag::Level::Debug => console_log(line),
    });

    let query = UrlQuery::from_page();
    let args = launch_args(&query);
    console::set_startup_args(args.clone());
    assets::set_games_root_override(PathBuf::from(
        query.get("games").unwrap_or(DEFAULT_GAMES_ROOT.into()),
    ));

    bootstrap::bench::arm();
    // Nothing is written yet: the web artifact sink (S5) replaces this directory.
    let artifacts = PathBuf::from("iw4l-artifacts");
    diag::init_log(&artifacts);
    diag::info!(Launch, "web launch args: {args:?}");
    let (mode, acceptance) =
        bootstrap::parse_cli(args.into_iter()).unwrap_or_else(|e| diag::exit_launch_error(&e));
    let games = assets::games_root_from_env().unwrap_or_else(|e| diag::exit_launch_error(&e));
    bootstrap::launch(games, artifacts, mode, acceptance);
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
    let map = query.get("map");
    let mode = query
        .get("mode")
        .unwrap_or_else(|| if map.is_some() { "map" } else { "menu" }.into());
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
