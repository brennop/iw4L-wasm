//! TEMPORARY (O4) browser master settings, read straight from the page URL so
//! the WebTransport backend can be tried before O5. O5 replaces this file:
//! the launcher turns URL parameters into a `MasterLaunchIntent` (join and
//! server browser, with the menu list), and the certificate hash moves into
//! `MasterTarget`.
//!
//! `?map=mp_rust&master=127.0.0.1:4435&master_hash=<hash_hex>&join=<room id>`
//! plus optional `master_password=` and `gametype=` (default `dm`).
//! `master` is `host:port` or a full `https://` URL; `master_hash` is
//! `hash_hex` from the master's `webtransport.json`; `join` is the room id
//! (`iw4l-master` logs it; `wt_smoke list` prints it).

use super::*;

fn query() -> Option<web_sys::UrlSearchParams> {
    let search = web_sys::window()?.location().search().ok()?;
    web_sys::UrlSearchParams::new_with_str(&search).ok()
}

fn param(query: &web_sys::UrlSearchParams, name: &str) -> Option<String> {
    query
        .get(name)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// O14: any URL parameter by name (diagnostic switches such as `up_drop`).
pub(super) fn query_param(name: &str) -> Option<String> {
    query().and_then(|query| param(&query, name))
}

/// The browser's master transport.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Transport {
    /// WebSocket (`conn_ws.rs`).
    Ws,
    /// WebTransport on the page's main thread (`conn_web.rs`).
    Wt,
    /// O19: WebTransport in a dedicated worker (`iw4l-wt-worker.js`), behind
    /// the ws backend's frames.
    WtWorker,
}

/// O16: WebSocket is the browser default; `?transport=wt` opts into
/// WebTransport for diagnostics, `?transport=wtw` (O19) into WebTransport in
/// a worker.
pub(super) fn transport() -> Transport {
    match query_param("transport").as_deref() {
        None | Some("ws") => Transport::Ws,
        Some("wt") => Transport::Wt,
        Some("wtw") => Transport::WtWorker,
        Some(other) => {
            diag::warn!(Net, "transport={other} is not ws|wt|wtw; using wt");
            Transport::Wt
        }
    }
}

/// D3a: the page's own origin as a ws URL, for a page the master serves.
fn origin_ws_url() -> Option<String> {
    let location = web_sys::window()?.location();
    let scheme = if location.protocol().ok()? == "https:" {
        "wss"
    } else {
        "ws"
    };
    Some(format!("{scheme}://{}/", location.host().ok()?))
}

/// `?master_ws=` when given, else (D3a) the page's origin: `?transport=ws`
/// alone is a complete link on a page served by the master's `--web-root`.
pub(super) fn master_ws_url() -> Option<String> {
    query_param("master_ws").or_else(origin_ws_url)
}

/// D3a: the first room of the origin's `/master.json` that is neither locked
/// nor full (a synchronous request: launch intent is built synchronously, and
/// the file is a few hundred bytes from the page's own server).
fn first_open_room() -> Result<String> {
    use js_sys::{Array, Reflect};
    let js = |error: wasm_bindgen::JsValue| format!("master.json: {error:?}");
    let request = web_sys::XmlHttpRequest::new().map_err(js)?;
    request
        .open_with_async("GET", "/master.json", false)
        .map_err(js)?;
    request.send().map_err(js)?;
    if request.status().map_err(js)? != 200 {
        return Err("no ?join= and /master.json is not served here".into());
    }
    let text = request
        .response_text()
        .map_err(js)?
        .ok_or("master.json: empty")?;
    let json = js_sys::JSON::parse(&text).map_err(js)?;
    let rooms = Array::from(&Reflect::get(&json, &"rooms".into()).map_err(js)?);
    let field = |room: &wasm_bindgen::JsValue, name: &str| {
        Reflect::get(room, &name.into()).unwrap_or(wasm_bindgen::JsValue::UNDEFINED)
    };
    for room in rooms.iter() {
        let open = field(&room, "locked").as_bool() == Some(false)
            && field(&room, "players").as_f64() < field(&room, "max_players").as_f64();
        if let (true, Some(id)) = (open, field(&room, "id").as_string()) {
            return Ok(id);
        }
    }
    Err("no ?join= and /master.json lists no open room".into())
}

/// The WebTransport URL for a `master` parameter.
fn webtransport_url(master: &str) -> String {
    if master.starts_with("https://") {
        master.to_owned()
    } else {
        format!("https://{master}/")
    }
}

/// A join intent when the URL has `master` and `join`; `None` when it has
/// neither (offline launch).
pub(super) fn join_from_query(map: &str, have: ContentFlags) -> Result<Option<MasterLaunchIntent>> {
    let Some(query) = query() else {
        return Ok(None);
    };
    // O16: a `?transport=ws` page may give only `master_ws` (the WebTransport
    // URL below is then unused).
    // D3a: with `?transport=ws` both may be left out on a page the master
    // serves: `master_ws` defaults to the page origin and `join` to the first
    // open room of its `/master.json`. (`wt` stays explicit: it needs the
    // port and certificate hash, which `/master.json` also has but this does
    // not read.)
    let ws = transport() == Transport::Ws;
    let master = param(&query, "master")
        .or_else(|| param(&query, "master_ws"))
        .or_else(|| ws.then(origin_ws_url).flatten());
    let Some(master) = master else {
        return Ok(None);
    };
    let join = match param(&query, "join") {
        Some(join) => join,
        None if ws => first_open_room()?,
        None => return Ok(None),
    };
    if cert_hash()?.is_none() {
        diag::warn!(
            Net,
            "master URL has no master_hash; the browser will only accept a Web-PKI certificate"
        );
    }
    let url = webtransport_url(&master);
    Ok(Some(MasterLaunchIntent(MasterLaunchMode::Join(
        JoinConfig {
            password: param(&query, "master_password").unwrap_or_else(|| "".to_owned()),
            target: MasterTarget {
                address: url.clone(),
                server_name: url,
                ca_pem: String::new(),
            },
            advert_id: join.parse()?,
            map: map.to_owned(),
            mode: param(&query, "gametype").unwrap_or_else(|| "dm".into()),
            have,
        },
    ))))
}

/// `master_hash` as 32 bytes (SHA-256 of the master's WebTransport
/// certificate), for `serverCertificateHashes`.
pub(super) fn cert_hash() -> Result<Option<[u8; 32]>> {
    let Some(hex) = query().and_then(|query| param(&query, "master_hash")) else {
        return Ok(None);
    };
    let hex: String = hex.chars().filter(|c| !matches!(c, ':' | ' ')).collect();
    if hex.len() != 64 || !hex.is_ascii() {
        return Err(format!("master_hash must be 64 hex digits, got `{hex}`").into());
    }
    let mut hash = [0_u8; 32];
    for (index, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
            .map_err(|error| format!("master_hash: {error}"))?;
    }
    Ok(Some(hash))
}
