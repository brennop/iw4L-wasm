use std::num::NonZero;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bevy::audio::{
    AddAudioSource, AudioSink, AudioSinkPlayback, Decodable, PlaybackMode, PlaybackSettings,
};
use bevy::prelude::*;
use rodio::mixer::{Mixer, MixerSource};
use rodio::{ChannelCount, Player, SampleRate, Source};

use crate::backend::{AudioScope, MatchEpoch, Voice};
use crate::pcm::{LoopingPcmAudio, PcmAudio};

const CHANNELS: ChannelCount = NonZero::new(2).unwrap();
const SAMPLE_RATE: SampleRate = NonZero::new(48_000).unwrap();

#[derive(Asset, TypePath)]
pub(crate) struct MatchBusAudio {
    source: Arc<Mutex<Option<MixerSource>>>,
}

pub(crate) struct MatchBusDecoder {
    source: Option<MixerSource>,
    available: Arc<Mutex<Option<MixerSource>>>,
    right: bool,
}

impl Iterator for MatchBusDecoder {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.source.is_none() && !self.right {
            self.source = self.available.lock().ok()?.take();
        }
        let sample = self.source.as_mut().and_then(Iterator::next).unwrap_or(0.0);
        self.right = !self.right;
        Some(sample)
    }
}

impl Drop for MatchBusDecoder {
    fn drop(&mut self) {
        if let Some(mut source) = self.source.take()
            && let Ok(mut available) = self.available.lock()
        {
            if self.right {
                source.next();
            }
            *available = Some(source);
        }
    }
}

impl Source for MatchBusDecoder {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> ChannelCount {
        CHANNELS
    }

    fn sample_rate(&self) -> SampleRate {
        SAMPLE_RATE
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

impl Decodable for MatchBusAudio {
    type Decoder = MatchBusDecoder;

    fn decoder(&self) -> Self::Decoder {
        MatchBusDecoder {
            source: self.source.lock().ok().and_then(|mut source| source.take()),
            available: Arc::clone(&self.source),
            right: false,
        }
    }
}

struct MatchBus {
    epoch: u64,
    entity: Entity,
    mixer: Mixer,
    ready: bool,
}

#[derive(Resource, Default)]
pub(crate) struct MatchBusState {
    bus: Option<MatchBus>,
}

impl MatchBusState {
    pub(crate) fn ready(&self, epoch: u64) -> bool {
        self.bus
            .as_ref()
            .is_some_and(|bus| bus.epoch == epoch && bus.ready)
    }
}

#[derive(Component)]
pub(crate) struct MatchBusVoice;

pub(crate) fn route_match_voices(
    mut commands: Commands,
    epoch: Res<MatchEpoch>,
    mut state: ResMut<MatchBusState>,
    mut buses: ResMut<Assets<MatchBusAudio>>,
    outputs: Query<&AudioSink, Without<Voice>>,
    voices: Query<
        (
            Entity,
            &Voice,
            &PlaybackSettings,
            Option<&AudioPlayer<PcmAudio>>,
            Option<&AudioPlayer<LoopingPcmAudio>>,
        ),
        Without<AudioSink>,
    >,
    pcm: Res<Assets<PcmAudio>>,
    looping: Res<Assets<LoopingPcmAudio>>,
) {
    if state.bus.as_ref().is_some_and(|bus| bus.epoch != epoch.0)
        && let Some(bus) = state.bus.take()
    {
        commands.entity(bus.entity).try_despawn();
    }
    if let Some(bus) = &mut state.bus {
        bus.ready = outputs.get(bus.entity).is_ok_and(|sink| !sink.empty());
    }
    for (entity, voice, settings, source, looped) in &voices {
        if voice.scope != AudioScope::Match || voice.epoch != epoch.0 || settings.spatial {
            continue;
        }
        let decoder = source
            .and_then(|source| pcm.get(&source.0))
            .map(Decodable::decoder)
            .or_else(|| {
                looped
                    .and_then(|source| looping.get(&source.0))
                    .map(Decodable::decoder)
            });
        let Some(decoder) = decoder else {
            continue;
        };
        let bus = state.bus.get_or_insert_with(|| {
            let (mixer, source) = rodio::mixer::mixer(CHANNELS, SAMPLE_RATE);
            let audio = buses.add(MatchBusAudio {
                source: Arc::new(Mutex::new(Some(source))),
            });
            let entity = commands
                .spawn((AudioPlayer(audio), PlaybackSettings::ONCE))
                .id();
            MatchBus {
                epoch: epoch.0,
                entity,
                mixer,
                ready: false,
            }
        });
        let (player, output) = Player::new();
        player.set_volume(settings.volume.to_linear());
        player.set_speed(settings.speed);
        if settings.paused {
            player.pause();
        }
        let mut decoder: Box<dyn Source + Send> = Box::new(decoder);
        if let Some(position) = settings.start_position {
            decoder = Box::new(decoder.skip_duration(position));
        }
        if let Some(duration) = settings.duration {
            decoder = Box::new(decoder.take_duration(duration));
        }
        if matches!(settings.mode, PlaybackMode::Loop) {
            decoder = Box::new(decoder.repeat_infinite());
        }
        player.append(decoder);
        let mut sink = AudioSink::new(player);
        if settings.muted {
            sink.mute();
        }
        bus.mixer.add(output);
        commands.entity(entity).insert((sink, MatchBusVoice));
    }
}

pub(crate) fn register(app: &mut App) {
    app.init_resource::<MatchBusState>()
        .add_audio_source::<MatchBusAudio>();
}
