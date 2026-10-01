use crate::ambient::{LegacyAmbient, MapAmbient, MapEmitter};
use crate::background::{namespace_alias, pick_variant};
use crate::{ClipStore, LoopingPcmAudio, PcmAudio, SoundBank, SoundPickState};
use bevy::ecs::system::SystemParam;
use bevy::{
    audio::{AudioSink, AudioSinkPlayback, PlaybackSettings, Volume},
    prelude::*,
};

#[derive(SystemParam)]
pub(crate) struct AmbientClock<'w> {
    game: Option<Res<'w, net::FrameClock>>,
    real: Res<'w, Time<Real>>,
}

#[derive(Resource, Default)]
pub(crate) struct ScriptAmbientPlayback {
    epoch: u64,
    target: Option<(bool, sim::ScriptAmbient)>,
    started: bool,
    variant: Option<usize>,
}

#[derive(Component)]
pub(crate) struct ScriptAmbientVoice {
    alias: String,
    base_gain: f32,
    variant: usize,
    from: f32,
    to: f32,
    start_ms: f64,
    end_ms: f64,
}

impl ScriptAmbientVoice {
    fn gain(&self, now: f64) -> f32 {
        if self.end_ms <= self.start_ms {
            return self.to;
        }
        let t = ((now - self.start_ms) / (self.end_ms - self.start_ms)).clamp(0.0, 1.0) as f32;
        self.from + (self.to - self.from) * t
    }
    fn fade(&mut self, now: f64, end: f64, to: f32) {
        self.from = self.gain(now);
        self.to = to;
        self.start_ms = now;
        self.end_ms = end.max(now);
    }
}

pub(crate) fn update_script_ambient(
    mut commands: Commands,
    epoch: Res<crate::backend::MatchEpoch>,
    ready: Res<crate::AudioReady>,
    loading: Option<Res<assets::LoadingScreen>>,
    presented: Option<Res<net::PresentedSnapshot>>,
    clock: AmbientClock,
    local: Res<net::LocalPresentClient>,
    bank: Option<Res<SoundBank>>,
    mut clips: Option<ResMut<ClipStore>>,
    mut looping: ResMut<Assets<LoopingPcmAudio>>,
    mut pcm: ResMut<Assets<PcmAudio>>,
    mut playback: ResMut<ScriptAmbientPlayback>,
    mut voices: Query<(
        Entity,
        &mut ScriptAmbientVoice,
        &mut PlaybackSettings,
        Option<&mut AudioSink>,
    )>,
    legacy: Query<Entity, (With<LegacyAmbient>, Without<MapEmitter>)>,
    mut gaps: ResMut<crate::MissingAliasGaps>,
    mut pick: ResMut<SoundPickState>,
) {
    if playback.epoch != epoch.0 {
        playback.epoch = epoch.0;
        playback.target = None;
        playback.started = false;
        playback.variant = None;
        let retired = !voices.is_empty();
        for (entity, _, _, _) in &voices {
            commands.entity(entity).try_despawn();
        }
        if retired {
            return;
        }
    }
    if !ready.0 || loading.is_some_and(|screen| !screen.is_complete()) {
        return;
    }
    let Some(game_clock) = clock.game.as_ref() else {
        return;
    };
    let Some(presented) = presented else {
        return;
    };
    let Some(snapshot) = presented.snapshot() else {
        return;
    };
    let ac130 = presented
        .player(local.0)
        .is_some_and(|ps| ps.other_flags & playerstate_iw4::other_flags::AC130 != 0);
    let target = if ac130 {
        &snapshot.meta.objectives.ac130_ambient
    } else {
        &snapshot.meta.objectives.ambient
    };
    let Some(plan) = target else {
        return;
    };
    for entity in &legacy {
        commands.entity(entity).try_despawn();
    }
    let now = clock.real.elapsed_secs_f64() * 1000.0;
    let end = now + f64::from(plan.end_ms.saturating_sub(game_clock.time()).max(0));
    let selected = (ac130, plan.clone());
    if playback.target.as_ref() != Some(&selected) {
        playback.started = false;
        playback.variant = plan.alias.as_ref().and_then(|alias| {
            let (namespace, name) = namespace_alias(alias);
            let sound = bank.as_ref()?.0.sound_in(namespace, name)?;
            pick_variant(sound, &mut pick, namespace, name)
        });
        let matching = voices.iter().any(|(_, voice, _, _)| {
            plan.alias.as_ref().is_some_and(|alias| {
                alias.eq_ignore_ascii_case(&voice.alias) && playback.variant == Some(voice.variant)
            })
        });
        let evict = if plan.alias.is_some() && !matching && voices.iter().count() >= 2 {
            voices
                .iter()
                .min_by(|a, b| {
                    a.1.to
                        .total_cmp(&b.1.to)
                        .then_with(|| a.1.gain(now).total_cmp(&b.1.gain(now)))
                })
                .map(|row| row.0)
        } else {
            None
        };
        for (entity, mut voice, _, _) in &mut voices {
            if Some(entity) == evict {
                commands.entity(entity).try_despawn();
                continue;
            }
            let keep = plan.alias.as_ref().is_some_and(|alias| {
                alias.eq_ignore_ascii_case(&voice.alias) && playback.variant == Some(voice.variant)
            });
            voice.fade(now, end, f32::from(u8::from(keep)));
            playback.started |= keep;
        }
        playback.target = Some(selected);
    }
    for (entity, voice, mut settings, sink) in &mut voices {
        let gain = voice.gain(now);
        if voice.to == 0.0 && now >= voice.end_ms {
            commands.entity(entity).try_despawn();
            continue;
        }
        settings.volume = Volume::Linear(voice.base_gain * gain);
        if let Some(mut sink) = sink {
            sink.set_volume(settings.volume);
        }
    }
    let Some(alias) = plan.alias.as_ref().filter(|_| !playback.started) else {
        return;
    };
    let (namespace, name) = namespace_alias(alias);
    let (Some(bank), Some(clips)) = (bank, clips.as_deref_mut()) else {
        return;
    };
    if playback.variant.is_none() {
        playback.variant = bank
            .0
            .sound_in(namespace, name)
            .and_then(|sound| pick_variant(sound, &mut pick, namespace, name));
    }
    let Some(variant) = playback.variant else {
        gaps.record(alias);
        return;
    };
    let Ok(Some(prepared)) =
        crate::background::prepare_background(alias, variant, &bank.0, clips, &mut pick, &mut gaps)
    else {
        return;
    };
    let crate::background::PreparedBackground {
        pcm: sound,
        looping: loops,
        gain,
        speed,
        channel,
    } = prepared;
    let voice = ScriptAmbientVoice {
        alias: alias.clone(),
        base_gain: gain,
        variant,
        from: 0.0,
        to: 1.0,
        start_ms: now,
        end_ms: end,
    };
    let volume = Volume::Linear(gain * voice.gain(now));
    let entity = if loops {
        let handle = looping.add(sound.into_looping());
        crate::backend::spawn_loop(
            &mut commands,
            handle,
            volume,
            epoch.0,
            crate::backend::AudioScope::Match,
        )
    } else {
        let handle = pcm.add(sound);
        crate::backend::spawn_oneshot(
            &mut commands,
            handle,
            volume,
            speed,
            epoch.0,
            crate::backend::AudioScope::Match,
        )
    };
    commands.entity(entity).insert((
        crate::backend::SoundChannel(channel),
        MapAmbient,
        voice,
        PlaybackSettings::ONCE.with_volume(volume).with_speed(speed),
    ));
    playback.started = true;
    pick.last_variant
        .insert((namespace, name.to_owned()), variant);
}
