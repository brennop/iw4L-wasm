use std::time::Duration;
use web_time::Instant;

use bevy::{
    audio::{AudioPlayer, PlaybackSettings, Volume},
    prelude::*,
};
use frame::ClientSet;

use crate::{AudioSink, AudioSinkPlayback};
use crate::pcm::{LoopingPcmAudio, LoopingPcmPlayback, PcmAudio};
use crate::voice::reclaim_finished_voices;

const STARTING_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MatchEpoch(pub u64);

impl MatchEpoch {
    pub fn bump(&mut self) {
        self.0 = self.0.wrapping_add(1);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioScope {
    Menu,
    Match,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VoiceKind {
    Oneshot,
    Loop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VoiceOwner {
    Exclusive,

    Attached,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VoicePhase {
    Starting,
    Playing,
}

#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SoundEntity(pub u32);

#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SoundChannel(pub u32);

#[derive(Component, Debug)]
pub struct Voice {
    pub epoch: u64,
    pub scope: AudioScope,
    kind: VoiceKind,
    owner: VoiceOwner,
    phase: VoicePhase,
    started_at: Instant,
}

pub(crate) fn register(app: &mut App) {
    app.init_resource::<MatchEpoch>().add_systems(
        Update,
        (cancel_stale_match_voices, advance_voice_phases)
            .chain()
            .before(reclaim_finished_voices)
            .in_set(ClientSet::Effects),
    );
}

pub(crate) fn spawn_oneshot(
    commands: &mut Commands,
    handle: Handle<PcmAudio>,
    volume: Volume,
    speed: f32,
    epoch: u64,
    scope: AudioScope,
) -> Entity {
    commands
        .spawn((
            AudioPlayer(handle),
            PlaybackSettings::ONCE.with_volume(volume).with_speed(speed),
            Voice {
                epoch,
                scope,
                kind: VoiceKind::Oneshot,
                owner: VoiceOwner::Exclusive,
                phase: VoicePhase::Starting,
                started_at: Instant::now(),
            },
        ))
        .id()
}

pub(crate) fn spawn_loop(
    commands: &mut Commands,
    handle: Handle<LoopingPcmAudio>,
    volume: Volume,
    epoch: u64,
    scope: AudioScope,
) -> Entity {
    commands
        .spawn((
            LoopingPcmPlayback::new(handle, volume),
            Voice {
                epoch,
                scope,
                kind: VoiceKind::Loop,
                owner: VoiceOwner::Exclusive,
                phase: VoicePhase::Starting,
                started_at: Instant::now(),
            },
        ))
        .id()
}

pub(crate) fn attach_loop(
    commands: &mut Commands,
    entity: Entity,
    handle: Handle<LoopingPcmAudio>,
    volume: Volume,
    epoch: u64,
) {
    commands.entity(entity).insert((
        LoopingPcmPlayback::new(handle, volume),
        Voice {
            epoch,
            scope: AudioScope::Match,
            kind: VoiceKind::Loop,
            owner: VoiceOwner::Attached,
            phase: VoicePhase::Starting,
            started_at: Instant::now(),
        },
    ));
}

pub(crate) fn detach_loop(commands: &mut Commands, entity: Entity) {
    commands.entity(entity).remove::<(
        AudioPlayer<LoopingPcmAudio>,
        PlaybackSettings,
        AudioSink,
        crate::match_bus::MatchBusVoice,
        Voice,
    )>();
}

pub(crate) fn stop(commands: &mut Commands, entity: Entity) {
    commands.entity(entity).try_despawn();
}

fn advance_voice_phases(
    mut voices: Query<(
        Entity,
        &mut Voice,
        Option<&AudioSink>,
        Option<&crate::match_bus::MatchBusVoice>,
    )>,
    bus: Option<Res<crate::match_bus::MatchBusState>>,
    mut commands: Commands,
) {
    let now = Instant::now();
    for (entity, mut voice, sink, routed) in &mut voices {
        if voice.kind == VoiceKind::Oneshot && sink.is_some_and(|sink| sink.empty()) {
            end_voice(&mut commands, entity, voice.owner);
            continue;
        }
        match voice.phase {
            VoicePhase::Starting => {
                if sink.is_some()
                    && (routed.is_none() || bus.as_ref().is_some_and(|bus| bus.ready(voice.epoch)))
                {
                    voice.phase = VoicePhase::Playing;
                    continue;
                }
                if now.duration_since(voice.started_at) >= STARTING_TIMEOUT {
                    diag::warn!(
                        Audio,
                        "audio: voice start timed out waiting for sink (typed gap)"
                    );
                    end_voice(&mut commands, entity, voice.owner);
                }
            }
            VoicePhase::Playing => {}
        }
    }
}

fn cancel_stale_match_voices(
    epoch: Res<MatchEpoch>,
    voices: Query<(Entity, &Voice)>,
    mut commands: Commands,
) {
    for (entity, voice) in &voices {
        if voice.scope != AudioScope::Match {
            continue;
        }
        if voice.epoch == epoch.0 {
            continue;
        }
        end_voice(&mut commands, entity, voice.owner);
    }
}

pub(crate) fn stop_entity_match(commands: &mut Commands, snd_ent: u32, epoch: u64) {
    commands.queue(move |world: &mut World| {
        let voices: Vec<_> = world
            .query::<(
                Entity,
                &Voice,
                Option<&crate::playback::AliasPlayback>,
                Option<&SoundEntity>,
            )>()
            .iter(world)
            .filter(|(_, voice, alias, source)| {
                voice.scope == AudioScope::Match
                    && voice.epoch == epoch
                    && (alias.is_some_and(|alias| alias.snd_ent == Some(snd_ent))
                        || source.is_some_and(|source| source.0 == snd_ent))
            })
            .map(|(entity, voice, _, _)| (entity, voice.owner))
            .collect();
        for (entity, owner) in voices {
            match owner {
                VoiceOwner::Exclusive => {
                    world.despawn(entity);
                }
                VoiceOwner::Attached => {
                    world.entity_mut(entity).remove::<(
                        AudioPlayer<LoopingPcmAudio>,
                        PlaybackSettings,
                        AudioSink,
                        crate::match_bus::MatchBusVoice,
                        Voice,
                    )>();
                }
            }
        }
    });
}

pub(crate) fn stop_all_match(commands: &mut Commands, epoch: u64) {
    commands.queue(move |world: &mut World| {
        let voices: Vec<_> = world
            .query::<(Entity, &Voice)>()
            .iter(world)
            .filter(|(_, voice)| voice.scope == AudioScope::Match && voice.epoch == epoch)
            .map(|(entity, voice)| (entity, voice.owner))
            .collect();
        for (entity, owner) in voices {
            match owner {
                VoiceOwner::Exclusive => {
                    world.despawn(entity);
                }
                VoiceOwner::Attached => {
                    world.entity_mut(entity).remove::<(
                        AudioPlayer<LoopingPcmAudio>,
                        PlaybackSettings,
                        AudioSink,
                        crate::match_bus::MatchBusVoice,
                        Voice,
                    )>();
                }
            }
        }
        if let Some(mut pending) = world.get_resource_mut::<crate::clip_store::PendingStarts>() {
            pending
                .entries
                .retain(|entry| entry.epoch != epoch || entry.class.scope() != AudioScope::Match);
        }
    });
}

fn end_voice(commands: &mut Commands, entity: Entity, owner: VoiceOwner) {
    match owner {
        VoiceOwner::Exclusive => {
            commands.entity(entity).try_despawn();
        }
        VoiceOwner::Attached => {
            detach_loop(commands, entity);
        }
    }
}
