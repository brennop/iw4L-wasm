//! Browser output (U8, experimental): voices mix on an AudioWorklet thread (`iw4l-mixer.js`)
//! instead of in bevy_audio/rodio/cpal on the main thread. The game keeps the same entity
//! model (`AudioPlayer<PcmAudio>` plus `AudioSink`); this module replaces the output behind it.
//! Clip PCM is registered once, each frame's voice changes go out as one `postMessage`, and the
//! worklet reports finished one-shots back. See `iw4l-mixer.js` for the message layout.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak as RcWeak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use bevy::{
    audio::{AudioPlayer, Decodable, GlobalVolume, PlaybackSettings, Volume},
    prelude::*,
};
use js_sys::{Array, Float32Array, Object, Reflect};
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use web_sys::{
    AudioContext, AudioContextState, AudioWorkletNode, AudioWorkletNodeOptions, MessageEvent,
};
use web_time::Instant;

use crate::pcm::{LivePan, LoopingPcmAudio, PcmAudio};

const CMD_START: f32 = 1.0;
const CMD_SET: f32 = 2.0;
const CMD_STOP: f32 = 3.0;
const FLAG_LOOP: f32 = 1.0;
const FLAG_PAUSED: f32 = 2.0;
const MAX_PENDING_MESSAGES: usize = 256;
const CLIP_SWEEP_FRAMES: u32 = 300;
const STATS_FRAMES: u32 = 60;

pub trait AudioSinkPlayback {
    fn volume(&self) -> Volume;
    fn set_volume(&mut self, volume: Volume);
    fn play(&self);
    fn pause(&self);
    fn is_paused(&self) -> bool;
    fn empty(&self) -> bool;
}

/// What bevy's `AudioSink` is natively: the handle of one playing voice.
#[derive(Component)]
pub struct AudioSink {
    voice: u32,
    volume: Volume,
    paused: AtomicBool,
    finished: bool,
    pan: Option<LivePan>,
    speed: f32,
    sent: Option<(f32, f32, bool)>,
}

impl AudioSinkPlayback for AudioSink {
    fn volume(&self) -> Volume {
        self.volume
    }

    fn set_volume(&mut self, volume: Volume) {
        self.volume = volume;
    }

    fn play(&self) {
        self.paused.store(false, Ordering::Relaxed);
    }

    fn pause(&self) {
        self.paused.store(true, Ordering::Relaxed);
    }

    fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    fn empty(&self) -> bool {
        self.finished
    }
}

struct Shared {
    node: Option<AudioWorkletNode>,
    pending: Vec<(JsValue, Array)>,
    failed: bool,
    finished: Vec<u32>,
    worklet_stats: (u32, u32),
}

struct ClipEntry {
    id: u32,
    samples: Weak<[f32]>,
}

struct Mixer {
    ctx: AudioContext,
    shared: Rc<RefCell<Shared>>,
    clips: HashMap<usize, ClipEntry>,
    new_clips: Vec<(u32, u16, u32, Vec<f32>)>,
    freed_clips: Vec<u32>,
    cmds: Vec<f32>,
    next_clip: u32,
    next_voice: u32,
    voice_entity: HashMap<u32, Entity>,
    live_voices: u32,
    messages: u64,
    last_stats: Instant,
    last_stats_messages: u64,
    frames: u32,
    warned_overflow: bool,
}

thread_local! {
    static MIXER: RefCell<Option<Mixer>> = const { RefCell::new(None) };
}

impl Mixer {
    fn new() -> Option<Self> {
        let ctx = match AudioContext::new() {
            Ok(ctx) => ctx,
            Err(err) => {
                diag::warn!(Audio, "audio: worklet mixer: AudioContext failed: {err:?}");
                return None;
            }
        };
        let shared = Rc::new(RefCell::new(Shared {
            node: None,
            pending: Vec::new(),
            failed: false,
            finished: Vec::new(),
            worklet_stats: (0, 0),
        }));
        let url = Reflect::get(&js_sys::global(), &"iw4lMixerUrl".into())
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_else(|| "./iw4l-mixer.js".to_owned());
        match ctx.audio_worklet().map(|w| w.add_module(&url)) {
            Ok(Ok(promise)) => {
                let ready = {
                    let ctx = ctx.clone();
                    let shared = Rc::downgrade(&shared);
                    Closure::once(move |_: JsValue| on_module_ready(&ctx, &shared))
                };
                let failed = {
                    let shared = Rc::downgrade(&shared);
                    Closure::once(move |err: JsValue| {
                        diag::warn!(Audio, "audio: worklet module failed to load: {err:?}");
                        if let Some(shared) = shared.upgrade() {
                            shared.borrow_mut().failed = true;
                        }
                    })
                };
                let _ = promise.then2(&ready, &failed);
                ready.forget();
                failed.forget();
            }
            _ => {
                diag::warn!(Audio, "audio: worklet mixer: audioWorklet unavailable");
                shared.borrow_mut().failed = true;
            }
        }
        diag::info!(
            Audio,
            "audio: worklet mixer started ({} Hz, baseLatency {:.1} ms)",
            ctx.sample_rate(),
            latency_of(&ctx, "baseLatency") * 1000.0
        );
        Some(Self {
            ctx,
            shared,
            clips: HashMap::new(),
            new_clips: Vec::new(),
            freed_clips: Vec::new(),
            cmds: Vec::new(),
            next_clip: 1,
            next_voice: 1,
            voice_entity: HashMap::new(),
            live_voices: 0,
            messages: 0,
            last_stats: Instant::now(),
            last_stats_messages: 0,
            frames: 0,
            warned_overflow: false,
        })
    }

    fn running(&self) -> bool {
        self.ctx.state() == AudioContextState::Running
    }

    fn clip_id(&mut self, pcm: &PcmAudio) -> u32 {
        let samples = pcm.samples();
        let key = Arc::as_ptr(samples) as *const f32 as usize;
        if let Some(entry) = self.clips.get(&key)
            && entry.samples.strong_count() > 0
        {
            return entry.id;
        }
        let id = self.next_clip;
        self.next_clip += 1;
        self.clips.insert(
            key,
            ClipEntry {
                id,
                samples: Arc::downgrade(samples),
            },
        );
        self.new_clips
            .push((id, pcm.channel_count(), pcm.rate(), samples.to_vec()));
        id
    }

    fn sweep_clips(&mut self) {
        let freed = &mut self.freed_clips;
        self.clips.retain(|_, entry| {
            let alive = entry.samples.strong_count() > 0;
            if !alive {
                freed.push(entry.id);
            }
            alive
        });
    }

    /// Sends what accumulated this frame as one message (or queues it until the node exists).
    fn flush(&mut self) {
        if self.new_clips.is_empty() && self.freed_clips.is_empty() && self.cmds.is_empty() {
            return;
        }
        let msg = Object::new();
        let transfer = Array::new();
        if !self.new_clips.is_empty() {
            let clips = Array::new();
            for (id, ch, rate, pcm) in self.new_clips.drain(..) {
                let data = Float32Array::from(pcm.as_slice());
                let buffer = data.buffer();
                let clip = Object::new();
                let _ = Reflect::set(&clip, &"id".into(), &id.into());
                let _ = Reflect::set(&clip, &"ch".into(), &ch.into());
                let _ = Reflect::set(&clip, &"rate".into(), &rate.into());
                let _ = Reflect::set(&clip, &"pcm".into(), &buffer);
                transfer.push(&buffer);
                clips.push(&clip);
            }
            let _ = Reflect::set(&msg, &"clips".into(), &clips);
        }
        if !self.freed_clips.is_empty() {
            let free: Array = self.freed_clips.drain(..).map(JsValue::from).collect();
            let _ = Reflect::set(&msg, &"free".into(), &free);
        }
        if !self.cmds.is_empty() {
            let cmds = Float32Array::from(self.cmds.as_slice());
            self.cmds.clear();
            let buffer = cmds.buffer();
            transfer.push(&buffer);
            let _ = Reflect::set(&msg, &"cmds".into(), &cmds);
        }
        self.messages += 1;
        let mut shared = self.shared.borrow_mut();
        match &shared.node {
            Some(node) => {
                let _ = node
                    .port()
                    .and_then(|port| port.post_message_with_transferable(&msg, &transfer));
            }
            None if shared.pending.len() < MAX_PENDING_MESSAGES => {
                shared.pending.push((msg.into(), transfer));
            }
            None => {
                if !self.warned_overflow {
                    self.warned_overflow = true;
                    diag::warn!(
                        Audio,
                        "audio: worklet not ready, dropping queued audio messages (typed gap)"
                    );
                }
            }
        }
    }

    fn publish_stats(&mut self) {
        let elapsed = self.last_stats.elapsed().as_secs_f64();
        let per_sec = (self.messages - self.last_stats_messages) as f64 / elapsed.max(1e-3);
        self.last_stats = Instant::now();
        self.last_stats_messages = self.messages;
        let shared = self.shared.borrow();
        let stats = Object::new();
        let set = |key: &str, value: JsValue| {
            let _ = Reflect::set(&stats, &key.into(), &value);
        };
        set("clips", (self.clips.len() as u32).into());
        set("voices", self.live_voices.into());
        set("workletVoices", shared.worklet_stats.0.into());
        set("workletClips", shared.worklet_stats.1.into());
        set("messages", (self.messages as f64).into());
        set("messagesPerSec", per_sec.into());
        set("pending", (shared.pending.len() as u32).into());
        set("ready", shared.node.is_some().into());
        set("failed", shared.failed.into());
        set("state", format!("{:?}", self.ctx.state()).to_lowercase().into());
        set("baseLatency", latency_of(&self.ctx, "baseLatency").into());
        set("outputLatency", latency_of(&self.ctx, "outputLatency").into());
        let _ = Reflect::set(&js_sys::global(), &"iw4lMixerStats".into(), &stats);
    }
}

/// `baseLatency` / `outputLatency` in seconds (0 where the browser lacks them).
fn latency_of(ctx: &AudioContext, key: &str) -> f64 {
    Reflect::get(ctx, &key.into())
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0)
}

fn on_module_ready(ctx: &AudioContext, shared: &RcWeak<RefCell<Shared>>) {
    let Some(shared) = shared.upgrade() else {
        return;
    };
    let options = AudioWorkletNodeOptions::new();
    options.set_number_of_inputs(0);
    options.set_number_of_outputs(1);
    options.set_output_channel_count(&Array::of1(&2.into()));
    let node = match AudioWorkletNode::new_with_options(ctx, "iw4l-mixer", &options) {
        Ok(node) => node,
        Err(err) => {
            diag::warn!(Audio, "audio: worklet node failed: {err:?}");
            shared.borrow_mut().failed = true;
            return;
        }
    };
    if let Err(err) = node.connect_with_audio_node(&ctx.destination()) {
        diag::warn!(Audio, "audio: worklet connect failed: {err:?}");
    }
    let Ok(port) = node.port() else {
        shared.borrow_mut().failed = true;
        return;
    };
    let inbox = Rc::downgrade(&shared);
    let onmessage = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
        let Some(shared) = inbox.upgrade() else {
            return;
        };
        let data = event.data();
        let mut shared = shared.borrow_mut();
        if let Ok(finished) = Reflect::get(&data, &"finished".into())
            && finished.is_object()
        {
            for id in Array::from(&finished).iter() {
                if let Some(id) = id.as_f64() {
                    shared.finished.push(id as u32);
                }
            }
        }
        if let Ok(stats) = Reflect::get(&data, &"stats".into())
            && stats.is_object()
        {
            let get = |key: &str| {
                Reflect::get(&stats, &key.into())
                    .ok()
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0) as u32
            };
            shared.worklet_stats = (get("voices"), get("clips"));
        }
    });
    port.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
    onmessage.forget();
    let mut inner = shared.borrow_mut();
    for (msg, transfer) in inner.pending.drain(..) {
        let _ = port.post_message_with_transferable(&msg, &transfer);
    }
    inner.node = Some(node);
}

/// Gives a clip entity its voice; implemented for the two asset kinds the game plays.
pub trait PcmSource: Asset + Decodable {
    fn pcm(&self) -> &PcmAudio;
    fn looping() -> bool;
}

impl PcmSource for PcmAudio {
    fn pcm(&self) -> &PcmAudio {
        self
    }

    fn looping() -> bool {
        false
    }
}

impl PcmSource for LoopingPcmAudio {
    fn pcm(&self) -> &PcmAudio {
        self.inner()
    }

    fn looping() -> bool {
        true
    }
}

fn pan_of(sink: &AudioSink) -> (f32, f32) {
    sink.pan.as_ref().map_or((1.0, 1.0), LivePan::get)
}

fn start_voices<T: PcmSource>(
    queued: Query<(Entity, &AudioPlayer<T>, &PlaybackSettings), Without<AudioSink>>,
    assets: Res<Assets<T>>,
    global: Res<GlobalVolume>,
    mut commands: Commands,
) {
    if queued.is_empty() {
        return;
    }
    MIXER.with_borrow_mut(|slot| {
        let Some(mixer) = slot.as_mut() else {
            return;
        };
        let running = mixer.running();
        for (entity, player, settings) in &queued {
            let Some(asset) = assets.get(&player.0) else {
                continue;
            };
            let pcm = asset.pcm();
            let voice = mixer.next_voice;
            mixer.next_voice += 1;
            let volume = settings.volume * global.volume;
            let mut sink = AudioSink {
                voice,
                volume,
                paused: AtomicBool::new(settings.paused),
                finished: false,
                pan: pcm.live_pan().cloned(),
                speed: settings.speed,
                sent: None,
            };
            // A one-shot started before the first gesture would queue up and all fire at once
            // on resume; loops (music, ambience) wait and start in place.
            if mixer.shared.borrow().failed || (!running && !T::looping()) {
                sink.finished = true;
                commands.entity(entity).insert(sink);
                continue;
            }
            let clip = mixer.clip_id(pcm);
            let (pl, pr) = pan_of(&sink);
            let g = volume.to_linear();
            let flags = if T::looping() { FLAG_LOOP } else { 0.0 }
                + if sink.paused.load(Ordering::Relaxed) { FLAG_PAUSED } else { 0.0 };
            mixer.cmds.extend_from_slice(&[
                CMD_START,
                voice as f32,
                clip as f32,
                flags,
                g * pl,
                g * pr,
                sink.speed,
            ]);
            sink.sent = Some((g * pl, g * pr, sink.paused.load(Ordering::Relaxed)));
            mixer.voice_entity.insert(voice, entity);
            mixer.live_voices += 1;
            commands.entity(entity).insert(sink);
        }
    });
}

fn sync_voices(
    mut sinks: Query<(Entity, &mut AudioSink)>,
    mut removed: RemovedComponents<AudioSink>,
) {
    MIXER.with_borrow_mut(|slot| {
        let Some(mixer) = slot.as_mut() else {
            return;
        };
        let finished: Vec<u32> = mixer.shared.borrow_mut().finished.drain(..).collect();
        for voice in finished {
            if let Some(entity) = mixer.voice_entity.remove(&voice) {
                mixer.live_voices = mixer.live_voices.saturating_sub(1);
                if let Ok((_, mut sink)) = sinks.get_mut(entity) {
                    sink.finished = true;
                }
            }
        }
        for entity in removed.read() {
            let gone = mixer
                .voice_entity
                .iter()
                .find_map(|(voice, e)| (*e == entity).then_some(*voice));
            if let Some(voice) = gone {
                mixer.voice_entity.remove(&voice);
                mixer.live_voices = mixer.live_voices.saturating_sub(1);
                mixer
                    .cmds
                    .extend_from_slice(&[CMD_STOP, voice as f32, 0.0, 0.0, 0.0, 0.0, 0.0]);
            }
        }
        for (_, mut sink) in &mut sinks {
            if sink.finished {
                continue;
            }
            let (pl, pr) = pan_of(&sink);
            let g = sink.volume.to_linear();
            let now = (g * pl, g * pr, sink.paused.load(Ordering::Relaxed));
            if sink.sent == Some(now) {
                continue;
            }
            sink.sent = Some(now);
            let flags = if now.2 { FLAG_PAUSED } else { 0.0 };
            mixer
                .cmds
                .extend_from_slice(&[CMD_SET, sink.voice as f32, 0.0, flags, now.0, now.1, sink.speed]);
        }
        mixer.frames += 1;
        if mixer.frames % CLIP_SWEEP_FRAMES == 0 {
            mixer.sweep_clips();
        }
        mixer.flush();
        if mixer.frames % STATS_FRAMES == 0 {
            mixer.publish_stats();
        }
    });
}

pub(crate) fn register(app: &mut App) {
    MIXER.with_borrow_mut(|slot| {
        if slot.is_none() {
            *slot = Mixer::new();
        }
    });
    app.init_resource::<GlobalVolume>()
        .init_asset::<PcmAudio>()
        .init_asset::<LoopingPcmAudio>()
        .add_systems(
            PostUpdate,
            (start_voices::<PcmAudio>, start_voices::<LoopingPcmAudio>, sync_voices).chain(),
        );
}
