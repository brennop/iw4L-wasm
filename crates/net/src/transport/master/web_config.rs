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
    let (Some(master), Some(join)) = (param(&query, "master"), param(&query, "join")) else {
        return Ok(None);
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
                ca_cert: None,
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
