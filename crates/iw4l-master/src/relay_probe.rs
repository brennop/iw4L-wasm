//! O13 per-hop datagram probes for the master (diagnostic only, behind
//! `IW4L_RELAY_PROBE=1`). The hash, the sample rule and the gap tracker match
//! `crates/net/src/transport/master/hop_probe.rs`; `hops.py` pairs the master's
//! `M.recv` / `M.send` lines with the browser's and the host's by payload hash.

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use master_protocol::{RelayDatagram, decode_relay};

use crate::peer_conn::PeerConnection;

/// A gap at least this long is logged.
const GAP_MS: u64 = 150;
/// Datagrams arriving within this window after a gap count as its burst.
const BURST_MS: u64 = 20;

pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("IW4L_RELAY_PROBE").is_ok_and(|v| v == "1"))
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn sampled(hash: u64) -> bool {
    hash.is_multiple_of(16)
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, PartialEq, Eq)]
struct GapEvent {
    gap_ms: u64,
    end_unix_ms: u64,
    burst: u32,
}

struct Burst {
    gap_ms: u64,
    end_unix_ms: u64,
    window_end: u64,
    count: u32,
}

/// Last-datagram time of one hop, direction and connection.
#[derive(Default)]
struct GapTracker {
    last: Option<u64>,
    burst: Option<Burst>,
}

impl GapTracker {
    /// Notes a datagram at `now` (Unix ms). Returns the gap events that are
    /// complete: a gap is reported when the first datagram after its burst
    /// window arrives, with the datagrams counted in that window.
    fn observe(&mut self, now: u64) -> Vec<GapEvent> {
        let mut done = Vec::new();
        if let Some(burst) = &mut self.burst {
            if now <= burst.window_end {
                burst.count += 1;
                self.last = Some(now);
                return done;
            }
            done.push(GapEvent {
                gap_ms: burst.gap_ms,
                end_unix_ms: burst.end_unix_ms,
                burst: burst.count,
            });
            self.burst = None;
        }
        if let Some(last) = self.last {
            let gap = now.saturating_sub(last);
            if gap >= GAP_MS {
                self.burst = Some(Burst {
                    gap_ms: gap,
                    end_unix_ms: now,
                    window_end: now + BURST_MS,
                    count: 1,
                });
            }
        }
        self.last = Some(now);
        done
    }
}

type TrackerKey = (&'static str, bool, usize);
static TRACKERS: Mutex<Option<HashMap<TrackerKey, GapTracker>>> = Mutex::new(None);

fn classify<'a>(relay: &RelayDatagram<'a>) -> (bool, &'a [u8]) {
    match relay {
        RelayDatagram::ClientToHost(payload) => (true, payload),
        RelayDatagram::ServiceToHost { payload, .. } => (true, payload),
        RelayDatagram::HostToMember { payload, .. } => (false, payload),
        RelayDatagram::ServiceToMember(payload) => (false, payload),
    }
}

fn dir_name(up: bool) -> &'static str {
    if up { "up" } else { "down" }
}

/// Probes one relay datagram at `hop` (`M.recv` / `M.send`) on connection
/// `conn` of transport `kind` (`wt` / `quic`).
pub fn probe(hop: &'static str, conn: usize, kind: &str, datagram: &[u8]) {
    if !enabled() {
        return;
    }
    let Ok(relay) = decode_relay(datagram) else {
        return;
    };
    let (up, payload) = classify(&relay);
    let now = unix_ms();
    let events = {
        let Ok(mut trackers) = TRACKERS.lock() else {
            return;
        };
        trackers
            .get_or_insert_with(HashMap::new)
            .entry((hop, up, conn))
            .or_default()
            .observe(now)
    };
    let mut err = std::io::stderr().lock();
    for event in events {
        let _ = writeln!(
            err,
            "hop gap: hop={hop} dir={} gap_ms={} end_unix_ms={} burst={} conn={conn} kind={kind}",
            dir_name(up),
            event.gap_ms,
            event.end_unix_ms,
            event.burst
        );
    }
    let hash = fnv1a64(payload);
    if sampled(hash) {
        let _ = writeln!(
            err,
            "hop probe: hop={hop} dir={} hash={hash:016x} len={} unix_ms={now} conn={conn} kind={kind}",
            dir_name(up),
            payload.len()
        );
    }
}

/// Logs the connection's QUIC stats every 5 s until it closes. Needs a tokio
/// runtime. For a WebTransport peer this is the underlying quinn connection
/// (wtransport's `quic_connection`).
pub fn watch(connection_id: u64, connection: &PeerConnection) {
    if !enabled() {
        return;
    }
    let (quic, kind) = match connection {
        PeerConnection::Quic(c) => (c.clone(), "quic"),
        PeerConnection::WebTransport(c) => (c.quic_connection().clone(), "wt"),
    };
    let conn = connection.stable_id();
    let _ = writeln!(
        std::io::stderr(),
        "relay probe: conn={conn} id={connection_id} kind={kind}"
    );
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            if quic.close_reason().is_some() {
                return;
            }
            let stats = quic.stats();
            let _ = writeln!(
                std::io::stderr(),
                "quic stats: conn={conn} kind={kind} rtt_ms={} cwnd={} lost_packets={} congestion_events={} sent_datagrams={} recv_datagrams={}",
                stats.path.rtt.as_millis(),
                stats.path.cwnd,
                stats.path.lost_packets,
                stats.path.congestion_events,
                stats.frame_tx.datagram,
                stats.frame_rx.datagram
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a64_vector() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn sample_rule() {
        assert!(sampled(0));
        assert!(sampled(0x10));
        assert!(!sampled(0x11));
        assert!(!sampled(15));
    }

    #[test]
    fn gap_and_burst() {
        let mut tracker = GapTracker::default();
        assert!(tracker.observe(1000).is_empty());
        assert!(tracker.observe(1010).is_empty());
        assert!(tracker.observe(1210).is_empty());
        assert!(tracker.observe(1215).is_empty());
        assert!(tracker.observe(1230).is_empty());
        let events = tracker.observe(1240);
        assert_eq!(
            events,
            vec![GapEvent {
                gap_ms: 200,
                end_unix_ms: 1210,
                burst: 3
            }]
        );
        assert!(tracker.observe(1389).is_empty());
        assert!(tracker.observe(1400).is_empty());
    }

    #[test]
    fn gap_right_after_flush() {
        let mut tracker = GapTracker::default();
        tracker.observe(0);
        tracker.observe(150);
        let events = tracker.observe(400);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].burst, 1);
        let events = tracker.observe(600);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].gap_ms, 250);
    }
}
