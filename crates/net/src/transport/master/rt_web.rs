//! Browser runtime for the master worker: the items `rt.rs` names, on the
//! page's single-threaded event loop. Tasks go to
//! `wasm_bindgen_futures::spawn_local`; timers are `setTimeout`.
//!
//! **Deadlines count loop-run time.** The page's main thread also runs the
//! engine, and the first frame of a map load blocks it for seconds. A plain
//! timer that expired during that frame would fire as soon as the loop
//! resumes, possibly before the network data that arrived meanwhile has been
//! delivered, and fail a healthy session. So `sleep` (and `timeout`,
//! `sleep_until`) advance in short steps and credit each step with at most
//! `STEP + STALL_SLACK` however late it woke: a stall costs a deadline at most
//! one capped step. When a deadline does run out, `timeout` gives the guarded
//! future one more turn of the event loop and polls it first (data before
//! deadline). Native timing is unchanged (tokio, `rt.rs`).

use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::future::{AbortHandle, Abortable};
use futures_util::stream::FuturesUnordered;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, spawn_local};

pub(super) use web_time::Instant;

/// Longest single timer behind `sleep`.
const STEP: Duration = Duration::from_millis(100);
/// Lateness a step may show and still count in full; anything later is a
/// stall of the page and is not counted.
const STALL_SLACK: Duration = Duration::from_millis(100);

/// Lateness at which `log_late` reports.
const LATE_LOG: Duration = Duration::from_millis(50);

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = setTimeout)]
    fn set_timeout(handler: &js_sys::Function, ms: i32) -> JsValue;
}

/// O13: logs a timer that fired 50 ms or more late (behind `pred_log`).
fn log_late(kind: &str, late: Duration) {
    if late >= LATE_LOG && crate::client::pred_log::enabled() {
        diag::info!(
            Net,
            "rt late: kind={kind} late_ms={} unix_ms={}",
            late.as_millis(),
            super::conn::hop_probe::unix_ms()
        );
    }
}

/// One `setTimeout` as a future. Dropping it leaves the timer to fire into a
/// promise nobody awaits.
fn delay(duration: Duration) -> JsFuture {
    let ms = duration.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
    JsFuture::from(js_sys::Promise::new(&mut |resolve, _reject| {
        set_timeout(&resolve, ms);
    }))
}

/// Waits `duration` of loop-run time (see the module note).
pub(super) async fn sleep(duration: Duration) {
    let mut left = duration;
    while !left.is_zero() {
        let step = left.min(STEP);
        let started = Instant::now();
        let _ = delay(step).await;
        log_late("sleep", started.elapsed().saturating_sub(step));
        left = left.saturating_sub(started.elapsed().min(step + STALL_SLACK));
    }
}

pub(super) async fn sleep_until(deadline: Instant) {
    sleep(deadline.saturating_duration_since(Instant::now())).await;
}

#[derive(Debug)]
pub(super) struct Elapsed;

impl fmt::Display for Elapsed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("deadline has elapsed")
    }
}

/// `future`, or `Elapsed` after `duration` of loop-run time. The future is
/// always polled before the deadline, and once more after one event-loop turn
/// when the deadline runs out.
pub(super) async fn timeout<F: Future>(
    duration: Duration,
    future: F,
) -> Result<F::Output, Elapsed> {
    let mut future = std::pin::pin!(future);
    tokio::select! {
        biased;
        output = &mut future => return Ok(output),
        () = sleep(duration) => {}
    }
    tokio::select! {
        biased;
        output = &mut future => Ok(output),
        _ = delay(Duration::ZERO) => Err(Elapsed),
    }
}

/// Only `Delay` exists: after a late tick the next one is a full period away,
/// so a stall never turns into a burst.
#[derive(Clone, Copy, Debug)]
pub(super) enum MissedTickBehavior {
    Delay,
}

pub(super) struct Interval {
    period: Duration,
    next: Instant,
    /// The pending timer for `next`, kept across dropped `tick` calls so a
    /// select loop that polls `tick` every turn does not start a timer each
    /// time.
    timer: Option<(Instant, JsFuture)>,
}

/// Like tokio's: the first tick completes at once.
pub(super) fn interval(period: Duration) -> Interval {
    Interval {
        period,
        next: Instant::now(),
        timer: None,
    }
}

impl Interval {
    pub(super) fn set_missed_tick_behavior(&mut self, behavior: MissedTickBehavior) {
        match behavior {
            MissedTickBehavior::Delay => {}
        }
    }

    pub(super) async fn tick(&mut self) -> Instant {
        loop {
            let now = Instant::now();
            if now >= self.next {
                log_late("interval", now - self.next);
                self.timer = None;
                self.next = now + self.period;
                return now;
            }
            let target = self.next;
            if !matches!(&self.timer, Some((at, _)) if *at == target) {
                self.timer = Some((target, delay(target - now)));
            }
            if let Some((_, timer)) = self.timer.as_mut() {
                let _ = timer.await;
            }
            self.timer = None;
        }
    }
}

/// A spawned task. Dropping it detaches the task (as tokio's handle and a
/// native worker thread do); `abort` stops it at its next await. `Send` and
/// `Sync` so it can sit in a Bevy resource as the worker handle.
pub(super) struct JoinHandle<T> {
    abort: AbortHandle,
    _output: PhantomData<fn() -> T>,
}

impl<T> JoinHandle<T> {
    pub(super) fn abort(&self) {
        self.abort.abort();
    }
}

pub(super) fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
{
    let (abort, registration) = AbortHandle::new_pair();
    spawn_local(async move {
        let _ = Abortable::new(future, registration).await;
    });
    JoinHandle {
        abort,
        _output: PhantomData,
    }
}

/// Runs `task()` on the page's event loop until it finishes. `name` is the
/// native thread's name; the browser has no thread to give it to. Never fails.
pub(super) fn spawn_worker<F, Fut>(_name: &str, task: F) -> std::io::Result<JoinHandle<()>>
where
    F: FnOnce() -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    Ok(spawn(task()))
}

#[derive(Debug)]
pub(super) struct JoinError;

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("task cancelled")
    }
}

/// tokio's `JoinSet` on `spawn_local`: tasks run whether or not `join_next` is
/// being polled, and dropping the set aborts them.
pub(super) struct JoinSet<T> {
    results: FuturesUnordered<tokio::sync::oneshot::Receiver<T>>,
    aborts: Vec<AbortHandle>,
}

impl<T: 'static> JoinSet<T> {
    pub(super) fn new() -> Self {
        Self {
            results: FuturesUnordered::new(),
            aborts: Vec::new(),
        }
    }

    pub(super) fn spawn<F>(&mut self, task: F)
    where
        F: Future<Output = T> + 'static,
    {
        let (done, result) = tokio::sync::oneshot::channel();
        let (abort, registration) = AbortHandle::new_pair();
        spawn_local(async move {
            if let Ok(output) = Abortable::new(task, registration).await {
                let _ = done.send(output);
            }
        });
        self.results.push(result);
        self.aborts.push(abort);
    }

    /// The next task to finish; `None` at once when the set is empty.
    pub(super) async fn join_next(&mut self) -> Option<Result<T, JoinError>> {
        self.results
            .next()
            .await
            .map(|result| result.map_err(|_| JoinError))
    }
}

impl<T> Drop for JoinSet<T> {
    fn drop(&mut self) {
        for abort in &self.aborts {
            abort.abort();
        }
    }
}
