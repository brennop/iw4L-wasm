use std::{
    any::Any,
    sync::Arc,
    time::{Duration, Instant},
};

use quinn_proto::{
    RttEstimator,
    congestion::{BbrConfig, Controller, ControllerFactory},
};

const STALL: Duration = Duration::from_secs(1);

#[derive(Debug, Default)]
pub struct StallTolerantConfig(BbrConfig);

impl ControllerFactory for StallTolerantConfig {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        Box::new(StallTolerant {
            inner: Arc::new(self.0.clone()).build(now, current_mtu),
            waiting_since: None,
            stalled: false,
        })
    }
}

struct StallTolerant {
    inner: Box<dyn Controller>,
    waiting_since: Option<Instant>,
    stalled: bool,
}

impl Controller for StallTolerant {
    fn on_sent(&mut self, now: Instant, bytes: u64, last_packet_number: u64) {
        let since = *self.waiting_since.get_or_insert(now);
        self.stalled = now.saturating_duration_since(since) >= STALL;
        self.inner.on_sent(now, bytes, last_packet_number);
    }

    fn on_ack(
        &mut self,
        now: Instant,
        sent: Instant,
        bytes: u64,
        app_limited: bool,
        rtt: &RttEstimator,
    ) {
        self.inner.on_ack(now, sent, bytes, app_limited, rtt);
    }

    fn on_end_acks(
        &mut self,
        now: Instant,
        in_flight: u64,
        app_limited: bool,
        largest_packet_num_acked: Option<u64>,
    ) {
        self.waiting_since = (in_flight > 0).then_some(now);
        self.stalled = false;
        self.inner
            .on_end_acks(now, in_flight, app_limited, largest_packet_num_acked);
    }

    fn on_congestion_event(
        &mut self,
        now: Instant,
        sent: Instant,
        is_persistent_congestion: bool,
        lost_bytes: u64,
    ) {
        self.inner
            .on_congestion_event(now, sent, is_persistent_congestion, lost_bytes);
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.inner.on_mtu_update(new_mtu);
    }

    fn window(&self) -> u64 {
        if self.stalled {
            u64::MAX
        } else {
            self.inner.window()
        }
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(StallTolerant {
            inner: self.inner.clone_box(),
            waiting_since: self.waiting_since,
            stalled: self.stalled,
        })
    }

    fn initial_window(&self) -> u64 {
        self.inner.initial_window()
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}
