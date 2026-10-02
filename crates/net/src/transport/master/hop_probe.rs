//! O13 per-hop datagram probes (diagnostic only, behind `pred_log::enabled()`).
//!
//! Every hop of the relay path hashes the inner payload of the datagram it
//! sees (`master_protocol::decode_relay`, the fragment bytes) with FNV-1a 64
//! and logs the datagrams whose hash is a multiple of 16. All hops apply the
//! same rule, so they sample the same datagrams without coordination, and
//! `hops.py` pairs the lines by hash. Unsampled, each hop and direction also
//! keeps the time of its last datagram and logs gaps of 150 ms or more with
//! the burst that follows. `iw4l-master/src/relay_probe.rs` carries the same
//! hash and rule.

use std::collections::HashMap;
use std::sync::Mutex;

use master_protocol::{RelayDatagram, decode_relay};

use crate::client::pred_log;

/// A gap at least this long is logged.
const GAP_MS: u64 = 150;
/// Datagrams arriving within this window after a gap count as its burst.
const BURST_MS: u64 = 20;

pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

pub(crate) fn sampled(hash: u64) -> bool {
    hash.is_multiple_of(16)
}

pub(crate) fn unix_ms() -> u64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
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

/// Last-datagram time of one hop and direction; reports gaps and bursts.
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

static TRACKERS: Mutex<Option<HashMap<(&'static str, bool), GapTracker>>> = Mutex::new(None);

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

/// Probes one relay datagram at `hop`. Returns the payload hash when the
/// datagram is sampled. `extra` is appended to the probe line.
pub(crate) fn probe(hop: &'static str, datagram: &[u8], extra: &str) -> Option<u64> {
    if !pred_log::enabled() {
        return None;
    }
    let relay = decode_relay(datagram).ok()?;
    let (up, payload) = classify(&relay);
    let now = unix_ms();
    let events = TRACKERS
        .lock()
        .ok()?
        .get_or_insert_with(HashMap::new)
        .entry((hop, up))
        .or_default()
        .observe(now);
    for event in events {
        diag::info!(
            Net,
            "hop gap: hop={hop} dir={} gap_ms={} end_unix_ms={} burst={}",
            dir_name(up),
            event.gap_ms,
            event.end_unix_ms,
            event.burst
        );
    }
    let hash = fnv1a64(payload);
    if !sampled(hash) {
        return None;
    }
    diag::info!(
        Net,
        "hop probe: hop={hop} dir={} hash={hash:016x} len={} unix_ms={now}{extra}",
        dir_name(up),
        payload.len()
    );
    Some(hash)
}

/// O14: whether an upstream relay datagram may be dropped when Chrome's
/// datagram queue is backed up. True only for a single-fragment `ClientToHost`
/// carrying `ClientPacket::Commands` (tag 2) or `SnapshotAck` (tag 3). Both are
/// resent by the client (unacked commands every send, the ack on every send),
/// and nothing else (actions, handshake, bootstrap) rides upstream datagrams.
/// The fragment header is `RF`, version, 0, index u16, count u16, id u32,
/// total_len u32 (`fragment.rs`).
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn upstream_droppable(datagram: &[u8]) -> bool {
    let Ok(RelayDatagram::ClientToHost(fragment)) = decode_relay(datagram) else {
        return false;
    };
    fragment.len() > 16
        && fragment[..2] == *b"RF"
        && fragment[4..6] == [0, 0]
        && fragment[6..8] == [1, 0]
        && matches!(fragment[16], 2 | 3)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay_of(packet: &crate::transport::protocol::ClientPacket) -> Vec<Vec<u8>> {
        use crate::transport::fragment::Fragmenter;
        let mut fragmenter = Fragmenter::new(master_protocol::MAX_OPAQUE_PAYLOAD, 256 * 1024);
        fragmenter
            .split(&packet.to_bytes())
            .unwrap()
            .iter()
            .map(|f| master_protocol::encode_relay(RelayDatagram::ClientToHost(f)).unwrap())
            .collect()
    }

    #[test]
    fn droppable_classification() {
        use crate::transport::protocol::{ClientPacket, ConnectionId, PacketHeader};
        let header = PacketHeader {
            connection: ConnectionId(7),
            sequence: 1,
            ack: 0,
            epoch: 0,
        };
        let ack = relay_of(&ClientPacket::SnapshotAck {
            header,
            snapshot_seq: 5,
        });
        assert_eq!(ack.len(), 1);
        assert!(upstream_droppable(&ack[0]));
        let cmds = relay_of(&ClientPacket::Commands {
            header,
            claimed_client: 1,
            cmds: Vec::new(),
            samples: Vec::new(),
            actions: Vec::new(),
            reliable_ack: 0,
        });
        assert!(upstream_droppable(&cmds[0]));
        // Not ClientToHost, not a fragment, or a multi-fragment message.
        let down =
            master_protocol::encode_relay(RelayDatagram::ServiceToMember(&ack[0][2..])).unwrap();
        assert!(!upstream_droppable(&down));
        assert!(!upstream_droppable(&[1, 1, 0, 0, 0]));
        let mut multi = ack[0].clone();
        multi[2 + 6] = 2;
        assert!(!upstream_droppable(&multi));
        let mut other_tag = ack[0].clone();
        other_tag[2 + 16] = 1;
        assert!(!upstream_droppable(&other_tag));
    }

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
        // 200 ms gap, then two more within 20 ms and one outside.
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
        // 149 ms is not a gap.
        assert!(tracker.observe(1389).is_empty());
        assert!(tracker.observe(1400).is_empty());
    }

    #[test]
    fn gap_right_after_flush() {
        let mut tracker = GapTracker::default();
        tracker.observe(0);
        tracker.observe(150);
        // The next datagram closes the burst and is itself after a new gap.
        let events = tracker.observe(400);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].burst, 1);
        let events = tracker.observe(600);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].gap_ms, 250);
    }
}
