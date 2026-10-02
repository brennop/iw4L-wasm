use std::sync::atomic::{AtomicBool, Ordering};

use bevy::prelude::*;

use crate::ServerTime;
use crate::authority::ack_log::percentile;
use crate::client::predict::PredictionMetrics;
use crate::client::runtime::{ClientPredictionState, LastAdoptedSnapshot, PendingClientSends};
use crate::schedule::ClientSet;

const WINDOW_SECS: f64 = 5.0;

static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn register(app: &mut App) {
    #[cfg(not(target_arch = "wasm32"))]
    if std::env::var("IW4L_PRED_LOG").is_ok_and(|value| value == "1") {
        enable();
    }
    app.add_systems(
        Update,
        log_prediction_window
            .in_set(ClientSet::Diag)
            .run_if(enabled),
    );
}

#[derive(Default)]
struct Window {
    start: Option<f64>,
    frames: u64,
    prev: PredictionMetrics,
    lead_ms: Vec<i32>,
    ack_age: Vec<i32>,
    newest_seq: Option<u32>,
    adopted_tick: Option<u32>,
}

fn unix_ms() -> u64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn log_prediction_window(
    time: Res<Time<Real>>,
    prediction: Res<ClientPredictionState>,
    adopted: Option<Res<LastAdoptedSnapshot>>,
    sends: Option<Res<PendingClientSends>>,
    mut window: Local<Window>,
) {
    let now = time.elapsed_secs_f64();
    let start = *window.start.get_or_insert(now);
    window.frames += 1;
    let adopted_tick = adopted.as_deref().and_then(|a| a.next()).map(|s| s.tick.0);
    if let (Some(cmd), Some(tick)) = (prediction.0.last_cmd(), adopted_tick) {
        window
            .lead_ms
            .push(cmd.server_time - ServerTime::from_tick(sim::Tick(tick)).ms());
    }
    window.ack_age.push(prediction.0.history().len() as i32);
    if let Some(cur) = sends.as_deref().and_then(|s| s.newest_seq()).map(|s| s.0) {
        let prev = window.newest_seq.unwrap_or(cur);
        if prev < cur && prev / 60 < cur / 60 {
            diag::info!(
                Net,
                "cmd probe client: seq={} unix_ms={}",
                cur / 60 * 60,
                unix_ms()
            );
        }
        window.newest_seq = Some(cur);
    }
    if let Some(cur) = adopted_tick {
        let prev = window.adopted_tick.unwrap_or(cur);
        if prev < cur && prev / 20 < cur / 20 {
            diag::info!(
                Net,
                "snap probe client: tick={} adopted_tick={cur} unix_ms={}",
                cur / 20 * 20,
                unix_ms()
            );
        }
        window.adopted_tick = Some(cur);
    }
    let secs = now - start;
    if secs < WINDOW_SECS {
        return;
    }
    let metrics = prediction.0.metrics();
    if let Some(line) = window_line(
        secs,
        window.frames,
        &window.prev,
        &metrics,
        prediction.0.history().len(),
    ) {
        let max = |v: &[i32]| v.iter().copied().max().unwrap_or(0);
        diag::info!(
            Net,
            "{line} cmd_lead_ms_p50={} cmd_lead_ms_max={} ack_age_p50={} ack_age_max={}",
            percentile(&window.lead_ms, 0.5),
            max(&window.lead_ms),
            percentile(&window.ack_age, 0.5),
            max(&window.ack_age)
        );
    }
    window.start = Some(now);
    window.frames = 0;
    window.prev = metrics;
    window.lead_ms.clear();
    window.ack_age.clear();
}

pub fn window_line(
    secs: f64,
    frames: u64,
    prev: &PredictionMetrics,
    now: &PredictionMetrics,
    history_len: usize,
) -> Option<String> {
    // A disarm rebuilds the prediction state and zeroes its counters.
    let base = if now.snapshots < prev.snapshots {
        PredictionMetrics::default()
    } else {
        *prev
    };
    let snapshots = now.snapshots - base.snapshots;
    if snapshots == 0 {
        return None;
    }
    let replayed = now.replayed_moves.saturating_sub(base.replayed_moves);
    Some(format!(
        "prediction window: secs={secs:.1} frames={frames} snaps={snapshots} replayed={replayed} replay_per_snap={:.2} acks={} dev={} forced={} evicted={} history={history_len} deepest={}",
        replayed as f64 / snapshots as f64,
        now.acks_matched.saturating_sub(base.acks_matched),
        now.deviations.saturating_sub(base.deviations),
        now.forced_adopts.saturating_sub(base.forced_adopts),
        now.evicted_moves.saturating_sub(base.evicted_moves),
        now.deepest_history,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_line_reports_deltas() {
        let prev = PredictionMetrics {
            snapshots: 100,
            replayed_moves: 400,
            acks_matched: 90,
            deepest_history: 6,
            ..Default::default()
        };
        let now = PredictionMetrics {
            snapshots: 200,
            replayed_moves: 950,
            acks_matched: 190,
            deviations: 2,
            deepest_history: 9,
            ..Default::default()
        };
        assert_eq!(
            window_line(5.0, 300, &prev, &now, 5).as_deref(),
            Some(
                "prediction window: secs=5.0 frames=300 snaps=100 replayed=550 replay_per_snap=5.50 acks=100 dev=2 forced=0 evicted=0 history=5 deepest=9"
            )
        );
    }

    #[test]
    fn window_line_skips_idle_and_survives_reset() {
        let prev = PredictionMetrics {
            snapshots: 50,
            ..Default::default()
        };
        assert_eq!(window_line(5.0, 1, &prev, &prev, 0), None);
        let reset = PredictionMetrics {
            snapshots: 4,
            replayed_moves: 8,
            ..Default::default()
        };
        assert!(
            window_line(5.0, 1, &prev, &reset, 0)
                .unwrap()
                .contains("snaps=4 replayed=8 replay_per_snap=2.00")
        );
    }
}
