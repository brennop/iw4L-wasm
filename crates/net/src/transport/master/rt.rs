//! The async runtime the master worker (`online.rs`) runs on. The worker takes
//! timers, task spawning and its background entry point from here, never from
//! `tokio::time`/`tokio::task` directly; `tokio::sync` and `select!` need no
//! runtime and are used as they are.
//!
//! Native: a current-thread tokio runtime on a named thread per worker, and
//! these are tokio's own items. A single-threaded backend (the browser,
//! `rt_web.rs`, selected here on wasm32) must keep their meaning: `spawn`
//! returns a handle whose `abort` stops the task, dropping a `JoinSet` aborts
//! what it holds, and `MissedTickBehavior::Delay` intervals do not burst after
//! a stall.

#[cfg(target_arch = "wasm32")]
pub(super) use super::rt_web::*;

#[cfg(not(target_arch = "wasm32"))]
use std::future::Future;
#[cfg(not(target_arch = "wasm32"))]
pub(super) use std::thread::JoinHandle;

#[cfg(not(target_arch = "wasm32"))]
pub(super) use tokio::spawn;
#[cfg(not(target_arch = "wasm32"))]
pub(super) use tokio::task::{JoinError, JoinSet};
#[cfg(not(target_arch = "wasm32"))]
pub(super) use tokio::time::{Instant, MissedTickBehavior, interval, sleep, sleep_until, timeout};

/// Runs `task()` in the background until it finishes. Native: on a thread
/// called `name`, with its own runtime; an error means the runtime could not
/// be built and nothing was started.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn spawn_worker<F, Fut>(name: &str, task: F) -> std::io::Result<JoinHandle<()>>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    Ok(std::thread::Builder::new()
        .name(name.into())
        .spawn(move || runtime.block_on(task()))
        .unwrap_or_else(|error| panic!("spawn {name} thread: {error}")))
}
