//! Host-side ack-latency diagnostics (O12), behind the `pred_log` flag.
//!
//! Classifies each remote client's command queue head once per authority
//! tick, and stamps wall-clock probes for every 60th command and every 20th
//! snapshot so a pairing script can split forward, queue and return legs.
//! Read-only: nothing here touches the queue or the clock.

use std::collections::HashMap;

use bevy::prelude::*;
use sim::ClientId;
use web_time::Instant;

use crate::authority::inbox::{
    AuthorityClock, ClientCommandInbox, MAX_COMMANDS_PER_PEER_PER_FRAME,
};
use crate::client::pred_log;
use crate::schedule::AuthoritySet;

const WINDOW_SECS: f64 = 5.0;
const CMD_PROBE_EVERY: u32 = 60;
const SNAP_PROBE_EVERY: u32 = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadKind {
    Empty,
    Ready,
    Future(i32),
    Gap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Classified {
    pub head: HeadKind,
    pub holes: u32,
    pub newest_age_ms: Option<i32>,
    pub len: usize,
}

/// Same rules as `ClientCommandInbox::take_for_tick`. `queue` is
/// `(seq, server_time)` in queue order; unsequenced rows are ignored.
pub fn classify(
    queue: impl Iterator<Item = (u32, i32)>,
    last_acked: u32,
    time_ms: i32,
) -> Classified {
    let expected = last_acked.wrapping_add(1);
    let rows: Vec<(u32, i32)> = queue.collect();
    let Some(&(head_seq, head_time)) = rows.first() else {
        return Classified {
            head: HeadKind::Empty,
            holes: 0,
            newest_age_ms: None,
            len: 0,
        };
    };
    let head = if head_seq != expected {
        HeadKind::Gap
    } else if head_time > time_ms {
        HeadKind::Future(head_time - time_ms)
    } else {
        HeadKind::Ready
    };
    let newest = rows.last().copied().unwrap_or((head_seq, head_time));
    let span = newest.0.wrapping_sub(expected);
    let holes = if span >= u32::MAX / 2 {
        0
    } else {
        let mut distinct = 0u32;
        let mut prev: Option<u32> = None;
        for &(seq, _) in &rows {
            if seq.wrapping_sub(expected) <= span && prev != Some(seq) {
                distinct += 1;
            }
            prev = Some(seq);
        }
        (span + 1).saturating_sub(distinct)
    };
    Classified {
        head,
        holes,
        newest_age_ms: Some(time_ms.saturating_sub(newest.1)),
        len: rows.len(),
    }
}

pub fn percentile(values: &[i32], p: f64) -> i32 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted[((sorted.len() - 1) as f64 * p).floor() as usize]
}

#[derive(Default)]
struct ClientWindow {
    ticks: u32,
    empty: u32,
    ready: u32,
    future: u32,
    gap: u32,
    holes: u32,
    capped: u32,
    queue_max: usize,
    future_ms: Vec<i32>,
    ages: Vec<i32>,
}

impl ClientWindow {
    fn note(&mut self, c: &Classified) {
        self.ticks += 1;
        match c.head {
            HeadKind::Empty => self.empty += 1,
            HeadKind::Ready => self.ready += 1,
            HeadKind::Future(ms) => {
                self.future += 1;
                self.future_ms.push(ms);
            }
            HeadKind::Gap => self.gap += 1,
        }
        self.holes += c.holes;
        if c.len > MAX_COMMANDS_PER_PEER_PER_FRAME {
            self.capped += 1;
        }
        self.queue_max = self.queue_max.max(c.len);
        if let Some(age) = c.newest_age_ms {
            self.ages.push(age);
        }
    }

    fn line(&self, client: u32) -> String {
        let max = |v: &[i32]| v.iter().copied().max().unwrap_or(0);
        format!(
            "ack window: client={client} ticks={} empty={} ready={} future={} future_ms_p50={} future_ms_max={} gap={} holes={} newest_age_ms_p50={} newest_age_ms_max={} queue_max={} capped={}",
            self.ticks,
            self.empty,
            self.ready,
            self.future,
            percentile(&self.future_ms, 0.5),
            max(&self.future_ms),
            self.gap,
            self.holes,
            percentile(&self.ages, 0.5),
            max(&self.ages),
            self.queue_max,
            self.capped,
        )
    }
}

#[derive(Default)]
struct Probe {
    seen_mark: u32,
    consumed_mark: u32,
}

#[derive(Resource, Default)]
pub struct AckLog {
    last_tick: Option<u32>,
    consume_tick: Option<u32>,
    last_snap_tick: Option<u32>,
    window_start: Option<Instant>,
    windows: HashMap<u32, ClientWindow>,
    probes: HashMap<u32, Probe>,
    clients: Vec<ClientId>,
}

fn unix_ms() -> u64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn classify_heads(
    clock: Res<AuthorityClock>,
    inbox: Res<ClientCommandInbox>,
    mut log: ResMut<AckLog>,
) {
    if log.last_tick == Some(clock.tick) {
        return;
    }
    log.last_tick = Some(clock.tick);
    log.consume_tick = Some(clock.tick);
    let now = Instant::now();
    let start = *log.window_start.get_or_insert(now);
    log.clients = inbox.known_clients();
    let clients = log.clients.clone();
    for id in clients {
        let last_acked = inbox.last_acked_seq(id).map_or(0, |s| s.0);
        let c = classify(
            inbox
                .peek_queue(id)
                .filter_map(|(seq, time)| seq.map(|s| (s.0, time))),
            last_acked,
            clock.time_ms,
        );
        let newest = inbox
            .peek_queue(id)
            .filter_map(|(seq, _)| seq.map(|s| s.0))
            .filter(|s| s.wrapping_sub(last_acked) < u32::MAX / 2)
            .max()
            .unwrap_or(0)
            .max(last_acked);
        let probe = log.probes.entry(id.0).or_insert_with(|| Probe {
            seen_mark: newest / CMD_PROBE_EVERY * CMD_PROBE_EVERY,
            consumed_mark: last_acked / CMD_PROBE_EVERY * CMD_PROBE_EVERY,
        });
        while probe.seen_mark + CMD_PROBE_EVERY <= newest {
            probe.seen_mark += CMD_PROBE_EVERY;
            diag::info!(
                Net,
                "cmd probe host: client={} seq={} event=seen tick={} unix_ms={}",
                id.0,
                probe.seen_mark,
                clock.tick,
                unix_ms()
            );
        }
        log.windows.entry(id.0).or_default().note(&c);
    }
    if now.duration_since(start).as_secs_f64() >= WINDOW_SECS {
        let mut ids: Vec<u32> = log.windows.keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            let w = &log.windows[&id];
            if w.ticks > 0 {
                diag::info!(Net, "{}", w.line(id));
            }
        }
        log.windows.clear();
        log.window_start = Some(now);
    }
}

fn consume_probe(
    clock: Res<AuthorityClock>,
    inbox: Res<ClientCommandInbox>,
    mut log: ResMut<AckLog>,
) {
    if log.consume_tick != Some(clock.tick) {
        return;
    }
    log.consume_tick = None;
    let clients = log.clients.clone();
    for id in clients {
        let Some(acked) = inbox.last_acked_seq(id).map(|s| s.0) else {
            continue;
        };
        let Some(probe) = log.probes.get_mut(&id.0) else {
            continue;
        };
        while probe.consumed_mark + CMD_PROBE_EVERY <= acked {
            probe.consumed_mark += CMD_PROBE_EVERY;
            diag::info!(
                Net,
                "cmd probe host: client={} seq={} event=consumed tick={} unix_ms={}",
                id.0,
                probe.consumed_mark,
                clock.tick,
                unix_ms()
            );
        }
    }
}

fn snapshot_probe(clock: Res<AuthorityClock>, mut log: ResMut<AckLog>) {
    if clock.tick == 0
        || !clock.tick.is_multiple_of(SNAP_PROBE_EVERY)
        || log.last_snap_tick == Some(clock.tick)
        || log.last_tick != Some(clock.tick)
    {
        return;
    }
    log.last_snap_tick = Some(clock.tick);
    diag::info!(
        Net,
        "snap probe host: tick={} unix_ms={}",
        clock.tick,
        unix_ms()
    );
}

pub fn register(app: &mut App) {
    app.init_resource::<AckLog>().add_systems(
        FixedUpdate,
        (
            classify_heads
                .after(AuthoritySet::Ingress)
                .before(AuthoritySet::Gather),
            consume_probe.after(AuthoritySet::Gather),
            snapshot_probe.after(AuthoritySet::Snapshot),
        )
            .run_if(pred_log::enabled),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_head() {
        let c = classify([].into_iter(), 4, 100);
        assert_eq!(c.head, HeadKind::Empty);
        let c = classify([(5, 90), (6, 100)].into_iter(), 4, 100);
        assert_eq!(
            (c.head, c.holes, c.newest_age_ms),
            (HeadKind::Ready, 0, Some(0))
        );
        let c = classify([(5, 130)].into_iter(), 4, 100);
        assert_eq!(c.head, HeadKind::Future(30));
        assert_eq!(c.newest_age_ms, Some(-30));
        let c = classify([(7, 90), (8, 95)].into_iter(), 4, 100);
        assert_eq!((c.head, c.holes, c.len), (HeadKind::Gap, 2, 2));
    }

    #[test]
    fn holes_inside_queue() {
        let c = classify([(5, 0), (6, 0), (9, 0)].into_iter(), 4, 100);
        assert_eq!((c.head, c.holes), (HeadKind::Ready, 2));
    }

    #[test]
    fn percentiles() {
        assert_eq!(percentile(&[], 0.5), 0);
        assert_eq!(percentile(&[5, 1, 3], 0.5), 3);
        assert_eq!(percentile(&[1, 2, 3, 4], 0.5), 2);
        assert_eq!(percentile(&[1, 2, 3, 4], 1.0), 4);
    }
}
