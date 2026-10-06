//! TEMPORARY (O4) browser master settings, read straight from the page URL so
//! the WebTransport backend can be tried before O5. O5 replaces this file:
//! the launcher turns URL parameters into a `MasterLaunchIntent` (join and
//! server browser, with the menu list), and the certificate hash moves into
//! `MasterTarget`.
//!
//! `?map=mp_rust&master=127.0.0.1:4435&master_hash=<hash_hex>&join=<room id>`
//! (WebTransport in a worker, O19; add `transport=ws&master_ws=ws://host:port/`
//! for WebSocket) plus optional `master_password=` and `gametype=` (default `dm`).
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

pub(super) use super::transport_pick::Transport;
use super::transport_pick::{WtInfo, pick_transport};

/// O19: WebTransport in a worker (`wtw`) is the default; WebSocket is the
/// fallback. An explicit `?transport=ws|wt|wtw` always wins. Without it, a
/// `master=` WebTransport address means `wtw`, `master_ws=` alone means `ws`,
/// and a bare link on a page the master serves follows its `/master.json`
/// (`wtw` when it publishes a WebTransport port, else `ws`). Decided once.
pub(super) fn transport() -> Transport {
    thread_local! {
        static PICK: Transport = {
            let pick = pick_transport(
                query_param("transport").as_deref(),
                query_param("master").is_some(),
                query_param("master_ws").is_some(),
                || master_json().map(|json| json.wt.clone()),
            );
            match &pick.note {
                Some(note) if query_param("transport").is_some() => diag::warn!(Net, "{note}"),
                Some(note) => diag::info!(Net, "{note}"),
                None => {}
            }
            pick.transport
        };
    }
    PICK.with(|pick| *pick)
}

/// What the page's `/master.json` says, read once (a synchronous request:
/// launch intent is built synchronously, and the file is a few hundred bytes
/// from the page's own server).
struct MasterJson {
    wt: Option<WtInfo>,
    first_open_room: Option<String>,
}

fn master_json() -> std::result::Result<&'static MasterJson, String> {
    thread_local! {
        static JSON: &'static std::result::Result<MasterJson, String> =
            Box::leak(Box::new(fetch_master_json()));
    }
    JSON.with(|json| json.as_ref().map_err(Clone::clone))
}

fn fetch_master_json() -> std::result::Result<MasterJson, String> {
    use js_sys::{Array, Reflect};
    let js = |error: wasm_bindgen::JsValue| format!("{error:?}");
    let request = web_sys::XmlHttpRequest::new().map_err(js)?;
    request
        .open_with_async("GET", "/master.json", false)
        .map_err(js)?;
    request.send().map_err(js)?;
    let status = request.status().map_err(js)?;
    if status != 200 {
        return Err(format!("/master.json status {status}"));
    }
    let text = request
        .response_text()
        .map_err(js)?
        .ok_or("master.json: empty")?;
    let json = js_sys::JSON::parse(&text).map_err(js)?;
    let field = |value: &wasm_bindgen::JsValue, name: &str| {
        Reflect::get(value, &name.into()).unwrap_or(wasm_bindgen::JsValue::UNDEFINED)
    };
    let webtransport = field(&json, "webtransport");
    let wt = match (
        field(&webtransport, "port").as_f64(),
        field(&webtransport, "hash_hex").as_string(),
    ) {
        (Some(port), Some(hash_hex)) if (1.0..=65535.0).contains(&port) => Some(WtInfo {
            port: port as u16,
            hash_hex,
            host: field(&webtransport, "host")
                .as_string()
                .filter(|host| !host.is_empty()),
        }),
        _ => None,
    };
    let rooms = Array::from(&field(&json, "rooms"));
    let first_open_room = rooms.iter().find_map(|room| {
        let open = field(&room, "locked").as_bool() == Some(false)
            && field(&room, "players").as_f64() < field(&room, "max_players").as_f64();
        open.then(|| field(&room, "id").as_string()).flatten()
    });
    Ok(MasterJson {
        wt,
        first_open_room,
    })
}

/// The WebTransport settings of `/master.json` when the page's WebTransport
/// address comes from there (no `?master=`, and the transport is `wt`/`wtw`).
fn master_json_wt() -> Option<WtInfo> {
    if query_param("master").is_some() || transport() == Transport::Ws {
        return None;
    }
    master_json().ok()?.wt.clone()
}

/// `https://<host>:<port>/`, the master's WebTransport listener from
/// `/master.json`: `webtransport.host` when the master publishes one (a page
/// behind a tunnel, whose hostname cannot carry UDP), else the page's own
/// hostname, whatever the page's own port.
fn master_json_wt_url() -> Option<String> {
    let wt = master_json_wt()?;
    let host = web_sys::window()?.location().hostname().ok()?;
    Some(wt.url(&host))
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
/// nor full.
fn first_open_room() -> Result<String> {
    let json = master_json().map_err(|error| format!("no ?join= and master.json: {error}"))?;
    json.first_open_room
        .clone()
        .ok_or_else(|| "no ?join= and /master.json lists no open room".into())
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
    // D3a/O19: on a page the master serves, both `master`/`master_ws` and
    // `join` may be left out: ws defaults to the page origin, wt/wtw to the
    // WebTransport listener `/master.json` publishes (see `transport`), and
    // `join` to the first open room of that file.
    let ws = transport() == Transport::Ws;
    let master = param(&query, "master")
        .or_else(|| ws.then(|| param(&query, "master_ws")).flatten())
        .or_else(|| ws.then(origin_ws_url).flatten())
        .or_else(|| (!ws).then(master_json_wt_url).flatten());
    let Some(master) = master else {
        return Ok(None);
    };
    let join = match param(&query, "join") {
        Some(join) => join,
        None if ws || master_json_wt().is_some() => first_open_room()?,
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
    // `master_hash=` wins; else the hash `/master.json` publishes, when the
    // WebTransport address came from there.
    let Some(hex) = query()
        .and_then(|query| param(&query, "master_hash"))
        .or_else(|| master_json_wt().map(|wt| wt.hash_hex))
    else {
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
