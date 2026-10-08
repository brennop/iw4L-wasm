//! Fork (wasm32): clip decode on the main thread. No prep thread can start in the browser, so a
//! store's jobs stay in its queue and `pump` decodes them a few milliseconds a frame (called by
//! web_output.rs). Results go where a worker's would: the outcomes, the resident cache and the
//! same counters. A child of `clip_store` so it reuses the worker's private helpers.

use std::cell::RefCell;
use std::sync::Weak;
use std::time::Duration;

use super::*;

struct InlineQueue {
    queue: Arc<MediaJobQueue>,
    bank: Arc<SoundCatalog>,
    iwd: Option<Arc<NamespaceSoundIwd>>,
    // Weak: the store owns these, and a dropped store retires its queue.
    outcomes: Weak<Mutex<Outcomes>>,
    requests: Weak<Mutex<MediaRequests>>,
    clip_cache: Option<PreparedClipCache>,
    common_profile_id: u64,
}

thread_local! {
    static QUEUES: RefCell<Vec<InlineQueue>> = const { RefCell::new(Vec::new()) };
}

pub(super) fn adopt(
    queue: &Arc<MediaJobQueue>,
    bank: &Arc<SoundCatalog>,
    iwd: Option<&Arc<NamespaceSoundIwd>>,
    outcomes: &Arc<Mutex<Outcomes>>,
    requests: &Arc<Mutex<MediaRequests>>,
    clip_cache: Option<&PreparedClipCache>,
    common_profile_id: u64,
) {
    diag::info!(
        Audio,
        "audio: no clip prep thread, clips decode inline on the main thread"
    );
    QUEUES.with_borrow_mut(|queues| {
        queues.push(InlineQueue {
            queue: Arc::clone(queue),
            bank: Arc::clone(bank),
            iwd: iwd.cloned(),
            outcomes: Arc::downgrade(outcomes),
            requests: Arc::downgrade(requests),
            clip_cache: clip_cache.cloned(),
            common_profile_id,
        });
    });
}

/// Decodes queued clips until `budget` is spent (at least one per call when any wait), newest
/// store first. A single clip is never split, so a call can overrun by one decode. Returns how
/// many clips it finished.
pub(crate) fn pump(budget: Duration) -> usize {
    let start = Instant::now();
    let mut done = 0;
    QUEUES.with_borrow_mut(|queues| {
        queues.retain(|queue| queue.outcomes.strong_count() > 0);
        for queue in queues.iter().rev() {
            let (Some(outcomes), Some(requests)) =
                (queue.outcomes.upgrade(), queue.requests.upgrade())
            else {
                continue;
            };
            loop {
                if done > 0 && start.elapsed() >= budget {
                    return;
                }
                let Some(job) = queue.queue.try_pop() else {
                    break;
                };
                decode(queue, &outcomes, &requests, job);
                done += 1;
            }
        }
    });
    done
}

fn decode(
    queue: &InlineQueue,
    outcomes: &Mutex<Outcomes>,
    requests: &Mutex<MediaRequests>,
    job: ClipJob,
) {
    QUEUE_WAIT_NS.fetch_add(job.queued_at.elapsed().as_nanos() as u64, Ordering::Relaxed);
    let prepare_at = Instant::now();
    let (path, mut result) = prepare_clip_now(&queue.bank, queue.iwd.as_deref(), &job.key);
    note_prepared(path, prepare_at, result.as_ref());
    if matches!(
        result,
        Err(ClipError::InvalidPcm(crate::media::PcmError::MemoryLimit))
    ) && evict_idle_streamed(requests, outcomes) != 0
    {
        let retry_at = Instant::now();
        let (path, retry) = prepare_clip_now(&queue.bank, queue.iwd.as_deref(), &job.key);
        note_prepared(path, retry_at, retry.as_ref());
        result = retry;
    }
    if let Some(cache) = &queue.clip_cache
        && let Ok(pcm) = &result
        && let Some((key, common)) = resident_clip_key(&queue.bank, &job.key)
    {
        cache.remember(queue.common_profile_id, key, common, pcm.clone());
    }
    let used = USE_TICK.fetch_add(1, Ordering::Relaxed);
    outcomes
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .insert(job.key, (result, used));
}
