//! O19: which master transport the browser uses when the page URL does not
//! name one. Plain data in, plain data out, so the rule is unit-tested on the
//! host; `web_config.rs` feeds it the URL and `/master.json`.

/// The browser's master transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Transport {
    /// WebSocket (`conn_ws.rs`).
    Ws,
    /// WebTransport on the page's main thread (`conn_web.rs`).
    Wt,
    /// O19: WebTransport in a dedicated worker (`iw4l-wt-worker.js`), behind
    /// the ws backend's frames.
    WtWorker,
}

/// The `webtransport` object of the master's `/master.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct WtInfo {
    pub(super) port: u16,
    pub(super) hash_hex: String,
}

/// The choice and, when it is a fallback or a mistake, one line saying why.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Pick {
    pub(super) transport: Transport,
    pub(super) note: Option<String>,
}

/// An explicit `?transport=` always wins (an unknown value warns and uses
/// `wt`). Without it: `master=` (a WebTransport address) means `wtw`; only
/// `master_ws=` means `ws`; a bare link on a page the master serves reads
/// `master_json` (called at most once, and only then) and uses `wtw` when the
/// master publishes a WebTransport port, else `ws`.
pub(super) fn pick_transport(
    param: Option<&str>,
    has_master: bool,
    has_master_ws: bool,
    master_json: impl FnOnce() -> Result<Option<WtInfo>, String>,
) -> Pick {
    let plain = |transport| Pick {
        transport,
        note: None,
    };
    match param {
        Some("ws") => plain(Transport::Ws),
        Some("wt") => plain(Transport::Wt),
        Some("wtw") => plain(Transport::WtWorker),
        Some(other) => Pick {
            transport: Transport::Wt,
            note: Some(format!("transport={other} is not ws|wt|wtw; using wt")),
        },
        None if has_master => plain(Transport::WtWorker),
        None if has_master_ws => plain(Transport::Ws),
        None => match master_json() {
            Ok(Some(_)) => plain(Transport::WtWorker),
            Ok(None) => Pick {
                transport: Transport::Ws,
                note: Some("master.json lists no WebTransport listener; using ws".to_owned()),
            },
            Err(error) => Pick {
                transport: Transport::Ws,
                note: Some(format!("master.json unavailable ({error}); using ws")),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wt() -> Result<Option<WtInfo>, String> {
        Ok(Some(WtInfo {
            port: 4435,
            hash_hex: "ab".repeat(32),
        }))
    }

    fn pick(
        param: Option<&str>,
        master: bool,
        master_ws: bool,
        json: Result<Option<WtInfo>, String>,
    ) -> Transport {
        pick_transport(param, master, master_ws, || json).transport
    }

    #[test]
    fn explicit_transport_wins() {
        assert_eq!(pick(Some("ws"), true, false, wt()), Transport::Ws);
        assert_eq!(pick(Some("wt"), false, true, wt()), Transport::Wt);
        assert_eq!(
            pick(Some("wtw"), false, true, Ok(None)),
            Transport::WtWorker
        );
        let odd = pick_transport(Some("quic"), false, false, wt);
        assert_eq!(odd.transport, Transport::Wt);
        assert!(odd.note.is_some());
    }

    #[test]
    fn master_param_means_wtw_master_ws_means_ws() {
        assert_eq!(pick(None, true, false, Ok(None)), Transport::WtWorker);
        assert_eq!(pick(None, true, true, Ok(None)), Transport::WtWorker);
        assert_eq!(pick(None, false, true, wt()), Transport::Ws);
    }

    #[test]
    fn bare_link_follows_master_json() {
        assert_eq!(pick(None, false, false, wt()), Transport::WtWorker);
        let none = pick_transport(None, false, false, || Ok(None));
        assert_eq!(none.transport, Transport::Ws);
        assert!(none.note.is_some());
        let err = pick_transport(None, false, false, || Err("HTTP 404".into()));
        assert_eq!(err.transport, Transport::Ws);
        assert!(err.note.unwrap().contains("HTTP 404"));
    }

    #[test]
    fn master_json_is_not_read_when_the_url_decides() {
        for (param, master, master_ws) in [
            (Some("ws"), false, false),
            (None, true, false),
            (None, false, true),
        ] {
            pick_transport(param, master, master_ws, || panic!("fetched"));
        }
    }
}
