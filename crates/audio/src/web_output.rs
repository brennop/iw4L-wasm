//! Fork (wasm32): browser audio output. Threads cannot spawn in the browser, so once a frame,
//! on the main thread, this decodes queued clips under a budget (inline_decode.rs), runs the
//! control pass the audio-control thread runs natively (runtime.rs), and renders just enough
//! 128-frame blocks to keep the AudioWorklet (`iw4l-mixer.js`, a ring-buffer player) a small
//! margin ahead of what it has played. The render frame therefore advances only as blocks are
//! rendered. A long frame runs the worklet dry and it plays silence: the next frame renders
//! at most `max_blocks`, it never bursts to catch up.
//!
//! The AudioContext starts suspended until a user gesture (index.html resumes it). Until it
//! runs, the control pass keeps the null transport on a wall clock, so one-shots started
//! before the gesture play into silence and are not queued up for later.
//!
//! URL knobs: `?audio_ms=<margin, default 60>`, `?audio_max_blocks=<per frame>`,
//! `?decode_ms=<inline decode budget per frame, default 4>`.
//! Stats: `window.iw4lMixerStats`, refreshed every `STATS_FRAMES` frames.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bevy::prelude::*;
use js_sys::{Array, Float32Array, Object, Reflect};
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use web_sys::{
    AudioContext, AudioContextOptions, AudioContextState, AudioWorkletNode,
    AudioWorkletNodeOptions, MessageEvent,
};
use web_time::Instant;

use crate::render_core::{QUANTUM, SAMPLE_RATE};
use crate::runtime::{ControlState, control_pass};

const DEFAULT_MARGIN_MS: f64 = 60.0;
const DEFAULT_DECODE_MS: f64 = 4.0;
const STATS_FRAMES: u32 = 30;

/// What the worklet last reported (cumulative counters) plus the level since the last
/// stats publish.
#[derive(Default)]
struct Report {
    reports: u64,
    consumed: f64,
    time: f64,
    buffered: f64,
    underruns: f64,
    underrun_frames: f64,
    overflow_frames: f64,
    peak: f64,
    sumsq: f64,
    n: f64,
}

#[derive(Default)]
struct Node {
    node: Option<AudioWorkletNode>,
    failed: bool,
}

/// Per-frame costs over one stats window.
#[derive(Default)]
struct Window {
    frames: u32,
    blocks: u64,
    max_blocks: usize,
    control_ms: f64,
    control_max: f64,
    render_ms: f64,
    render_max: f64,
    decode_ms: f64,
    decode_max: f64,
    frame_ms: f64,
    frame_max: f64,
}

#[derive(Default)]
struct Totals {
    frames: u64,
    running_frames: u64,
    blocks: u64,
    control_ms: f64,
    render_ms: f64,
    decode_ms: f64,
    decoded: u64,
    starved: u64,
}

struct WebOutput {
    state: ControlState,
    shutdown: Arc<AtomicBool>,
    ctx: Option<AudioContext>,
    node: Rc<RefCell<Node>>,
    report: Rc<RefCell<Report>>,
    margin_frames: u64,
    max_blocks: usize,
    decode_budget: Duration,
    posted: u64,
    running: bool,
    scratch: Vec<f32>,
    block: [[f32; 2]; QUANTUM],
    last_frame: Option<Instant>,
    window: Window,
    totals: Totals,
}

thread_local! {
    static OUTPUT: RefCell<Option<WebOutput>> = const { RefCell::new(None) };
}

/// Takes over the control state `AudioRuntime::new` would hand to its thread.
pub(crate) fn adopt(state: ControlState, shutdown: Arc<AtomicBool>, device_enabled: bool) {
    let margin_ms = query_f64("audio_ms")
        .filter(|ms| (5.0..=1000.0).contains(ms))
        .unwrap_or(DEFAULT_MARGIN_MS);
    let margin_frames = (margin_ms * f64::from(SAMPLE_RATE) / 1000.0).ceil() as u64;
    let max_blocks = query_f64("audio_max_blocks")
        .filter(|blocks| (1.0..=512.0).contains(blocks))
        .map_or(margin_frames.div_ceil(QUANTUM as u64) as usize + 2, |blocks| {
            blocks as usize
        });
    let decode_ms = query_f64("decode_ms")
        .filter(|ms| (0.0..=100.0).contains(ms))
        .unwrap_or(DEFAULT_DECODE_MS);
    let node = Rc::new(RefCell::new(Node::default()));
    let report = Rc::new(RefCell::new(Report::default()));
    let ctx = if device_enabled {
        open(&node, &report)
    } else {
        None
    };
    diag::info!(
        Audio,
        "audio: browser output margin={margin_ms:.0}ms max_blocks={max_blocks} decode_budget={decode_ms:.1}ms context={}",
        ctx.is_some()
    );
    OUTPUT.with_borrow_mut(|slot| {
        *slot = Some(WebOutput {
            state,
            shutdown,
            ctx,
            node,
            report,
            margin_frames,
            max_blocks,
            decode_budget: Duration::from_secs_f64(decode_ms / 1000.0),
            posted: 0,
            running: false,
            scratch: Vec::with_capacity(max_blocks * QUANTUM * 2),
            block: [[0.0; 2]; QUANTUM],
            last_frame: None,
            window: Window::default(),
            totals: Totals::default(),
        });
    });
}

pub(crate) fn register(app: &mut App) {
    app.add_systems(Last, web_audio_frame);
}

fn web_audio_frame() {
    OUTPUT.with_borrow_mut(|slot| {
        let Some(output) = slot.as_mut() else {
            return;
        };
        if output.shutdown.load(Ordering::Acquire) {
            *slot = None;
            return;
        }
        output.frame();
    });
}

fn ms(from: Instant, to: Instant) -> f64 {
    to.saturating_duration_since(from).as_secs_f64() * 1000.0
}

impl WebOutput {
    fn context_running(&self) -> bool {
        self.node.borrow().node.is_some()
            && self
                .ctx
                .as_ref()
                .is_some_and(|ctx| ctx.state() == AudioContextState::Running)
    }

    fn frame(&mut self) {
        let start = Instant::now();
        if let Some(last) = self.last_frame.replace(start) {
            let frame_ms = ms(last, start);
            self.window.frame_ms += frame_ms;
            self.window.frame_max = self.window.frame_max.max(frame_ms);
        }
        let decoded = crate::clip_store::inline_decode::pump(self.decode_budget);
        let decoded_at = Instant::now();

        let running = self.context_running();
        if running != self.running {
            self.running = running;
            diag::info!(
                Audio,
                "audio: browser output {} at audio frame {}",
                if running { "running" } else { "stopped" },
                self.state.shared.frame.load(Ordering::Acquire)
            );
        }
        self.state
            .shared
            .device_active
            .store(running, Ordering::Release);
        control_pass(&mut self.state, decoded_at);
        let controlled_at = Instant::now();

        let blocks = if running { self.render() } else { 0 };
        let rendered_at = Instant::now();

        let decode_ms = ms(start, decoded_at);
        let control_ms = ms(decoded_at, controlled_at);
        let render_ms = ms(controlled_at, rendered_at);
        let window = &mut self.window;
        window.frames += 1;
        window.blocks += blocks as u64;
        window.max_blocks = window.max_blocks.max(blocks);
        window.decode_ms += decode_ms;
        window.decode_max = window.decode_max.max(decode_ms);
        window.control_ms += control_ms;
        window.control_max = window.control_max.max(control_ms);
        window.render_ms += render_ms;
        window.render_max = window.render_max.max(render_ms);
        let totals = &mut self.totals;
        totals.frames += 1;
        totals.running_frames += u64::from(running);
        totals.blocks += blocks as u64;
        totals.decode_ms += decode_ms;
        totals.control_ms += control_ms;
        totals.render_ms += render_ms;
        totals.decoded += decoded as u64;
        if self.window.frames >= STATS_FRAMES {
            self.publish_stats();
            self.window = Window::default();
        }
    }

    /// Frames posted to the worklet that it has not played yet, extrapolated from its last
    /// report along the audio clock.
    fn buffered(&self) -> f64 {
        let report = self.report.borrow();
        let consumed = if report.reports == 0 {
            0.0
        } else {
            let since = self
                .ctx
                .as_ref()
                .map_or(0.0, |ctx| (ctx.current_time() - report.time).max(0.0));
            report.consumed + since * f64::from(SAMPLE_RATE)
        };
        (self.posted as f64 - consumed).max(0.0)
    }

    fn render(&mut self) -> usize {
        let buffered = self.buffered();
        if buffered < QUANTUM as f64 && self.posted > 0 {
            self.totals.starved += 1;
        }
        let want = (self.margin_frames as f64 - buffered).max(0.0);
        let blocks = ((want / QUANTUM as f64).ceil() as usize).min(self.max_blocks);
        if blocks == 0 {
            return 0;
        }
        self.scratch.clear();
        for _ in 0..blocks {
            self.state
                .shared
                .render_for(&mut self.block, Some(true));
            self.scratch.extend(self.block.iter().flatten());
        }
        let node = self.node.borrow();
        let Some(port) = node.node.as_ref().and_then(|node| node.port().ok()) else {
            return 0;
        };
        let pcm = Float32Array::from(self.scratch.as_slice());
        let msg = Object::new();
        let _ = Reflect::set(&msg, &"pcm".into(), &pcm);
        if port
            .post_message_with_transferable(&msg, &Array::of1(&pcm.buffer()))
            .is_ok()
        {
            self.posted += (blocks * QUANTUM) as u64;
        }
        blocks
    }

    fn publish_stats(&mut self) {
        let stats = Object::new();
        let set = |key: &str, value: f64| {
            let _ = Reflect::set(&stats, &key.into(), &JsValue::from_f64(value));
        };
        let frames = f64::from(self.window.frames.max(1));
        let buffered = self.buffered();
        {
            let mut report = self.report.borrow_mut();
            set("consumed", report.consumed);
            set("workletBufferedMs", report.buffered * 1000.0 / f64::from(SAMPLE_RATE));
            set("underruns", report.underruns);
            set("underrunFrames", report.underrun_frames);
            set("overflowFrames", report.overflow_frames);
            set("reports", report.reports as f64);
            set("peak", report.peak);
            set(
                "rms",
                if report.n > 0.0 {
                    (report.sumsq / (2.0 * report.n)).sqrt()
                } else {
                    0.0
                },
            );
            report.peak = 0.0;
            report.sumsq = 0.0;
            report.n = 0.0;
        }
        set("posted", self.posted as f64);
        set("bufferedMs", buffered * 1000.0 / f64::from(SAMPLE_RATE));
        set(
            "marginMs",
            self.margin_frames as f64 * 1000.0 / f64::from(SAMPLE_RATE),
        );
        set("blocksPerFrame", self.window.blocks as f64 / frames);
        set("maxBlocksFrame", self.window.max_blocks as f64);
        set("controlMsAvg", self.window.control_ms / frames);
        set("controlMsMax", self.window.control_max);
        set("renderMsAvg", self.window.render_ms / frames);
        set("renderMsMax", self.window.render_max);
        set("decodeMsAvg", self.window.decode_ms / frames);
        set("decodeMsMax", self.window.decode_max);
        set("frameMsAvg", self.window.frame_ms / frames);
        set("frameMsMax", self.window.frame_max);
        set("totalFrames", self.totals.frames as f64);
        set("totalRunningFrames", self.totals.running_frames as f64);
        set("totalBlocks", self.totals.blocks as f64);
        set("totalControlMs", self.totals.control_ms);
        set("totalRenderMs", self.totals.render_ms);
        set("totalDecodeMs", self.totals.decode_ms);
        set("decodedClips", self.totals.decoded as f64);
        set("starvedFrames", self.totals.starved as f64);
        let shared = &self.state.shared;
        set("audioFrame", shared.frame.load(Ordering::Acquire) as f64);
        set("deviceBlocks", shared.device_blocks.load(Ordering::Relaxed) as f64);
        set("nullBlocks", shared.null_blocks.load(Ordering::Relaxed) as f64);
        set(
            "renderPeak",
            f64::from(f32::from_bits(shared.peak.load(Ordering::Relaxed))),
        );
        let (instances, started) = self.state.voices();
        set("instances", instances as f64);
        set("voices", started as f64);
        let node = self.node.borrow();
        let _ = Reflect::set(&stats, &"ready".into(), &node.node.is_some().into());
        let _ = Reflect::set(&stats, &"failed".into(), &node.failed.into());
        let _ = Reflect::set(&stats, &"running".into(), &self.running.into());
        let state = self.ctx.as_ref().map_or("none".to_owned(), |ctx| {
            format!("{:?}", ctx.state()).to_lowercase()
        });
        let _ = Reflect::set(&stats, &"state".into(), &state.into());
        if let Some(ctx) = &self.ctx {
            set("sampleRate", f64::from(ctx.sample_rate()));
            set("baseLatency", latency_of(ctx, "baseLatency"));
            set("outputLatency", latency_of(ctx, "outputLatency"));
        }
        let _ = Reflect::set(&js_sys::global(), &"iw4lMixerStats".into(), &stats);
    }
}

/// `baseLatency` / `outputLatency` in seconds (0 where the browser lacks them).
fn latency_of(ctx: &AudioContext, key: &str) -> f64 {
    Reflect::get(ctx, &key.into())
        .ok()
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0)
}

fn query_f64(name: &str) -> Option<f64> {
    let search = web_sys::window()?.location().search().ok()?;
    let params = web_sys::UrlSearchParams::new_with_str(&search).ok()?;
    params.get(name)?.trim().parse().ok()
}

fn open(node: &Rc<RefCell<Node>>, report: &Rc<RefCell<Report>>) -> Option<AudioContext> {
    let options = AudioContextOptions::new();
    options.set_sample_rate(SAMPLE_RATE as f32);
    let ctx = match AudioContext::new_with_context_options(&options) {
        Ok(ctx) => ctx,
        Err(error) => {
            diag::warn!(Audio, "audio: AudioContext failed: {error:?}");
            return None;
        }
    };
    let url = Reflect::get(&js_sys::global(), &"iw4lMixerUrl".into())
        .ok()
        .and_then(|value| value.as_string())
        .unwrap_or_else(|| "./iw4l-mixer.js".to_owned());
    match ctx.audio_worklet().and_then(|worklet| worklet.add_module(&url)) {
        Ok(promise) => {
            let ready = {
                let ctx = ctx.clone();
                let node = node.clone();
                let report = report.clone();
                Closure::once(move |_: JsValue| on_module_ready(&ctx, &node, &report))
            };
            let failed = {
                let node = node.clone();
                Closure::once(move |error: JsValue| {
                    diag::warn!(Audio, "audio: worklet module failed to load: {error:?}");
                    node.borrow_mut().failed = true;
                })
            };
            let _ = promise.then2(&ready, &failed);
            ready.forget();
            failed.forget();
        }
        Err(error) => {
            diag::warn!(Audio, "audio: audioWorklet unavailable: {error:?}");
            node.borrow_mut().failed = true;
        }
    }
    Some(ctx)
}

fn on_module_ready(ctx: &AudioContext, node: &Rc<RefCell<Node>>, report: &Rc<RefCell<Report>>) {
    let options = AudioWorkletNodeOptions::new();
    options.set_number_of_inputs(0);
    options.set_number_of_outputs(1);
    options.set_output_channel_count(&Array::of1(&2.into()));
    let worklet = match AudioWorkletNode::new_with_options(ctx, "iw4l-mixer", &options) {
        Ok(worklet) => worklet,
        Err(error) => {
            diag::warn!(Audio, "audio: worklet node failed: {error:?}");
            node.borrow_mut().failed = true;
            return;
        }
    };
    if let Err(error) = worklet.connect_with_audio_node(&ctx.destination()) {
        diag::warn!(Audio, "audio: worklet connect failed: {error:?}");
    }
    let Ok(port) = worklet.port() else {
        node.borrow_mut().failed = true;
        return;
    };
    let inbox = Rc::downgrade(report);
    let onmessage = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
        let Some(report) = inbox.upgrade() else {
            return;
        };
        let data = event.data();
        let get = |key: &str| {
            Reflect::get(&data, &key.into())
                .ok()
                .and_then(|value| value.as_f64())
                .unwrap_or(0.0)
        };
        let mut report = report.borrow_mut();
        report.reports += 1;
        report.consumed = get("consumed");
        report.time = get("time");
        report.buffered = get("buffered");
        report.underruns = get("underruns");
        report.underrun_frames = get("underrunFrames");
        report.overflow_frames = get("overflowFrames");
        report.peak = report.peak.max(get("peak"));
        report.sumsq += get("sumsq");
        report.n += get("n");
    });
    port.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
    onmessage.forget();
    diag::info!(
        Audio,
        "audio: worklet player ready ({} Hz, baseLatency {:.1} ms)",
        ctx.sample_rate(),
        latency_of(ctx, "baseLatency") * 1000.0
    );
    node.borrow_mut().node = Some(worklet);
}
