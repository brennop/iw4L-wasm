use crate::background::{namespace_alias, pick_variant, prepare_background};
use crate::{ClipStore, LoopingPcmAudio, PcmAudio, SoundBank, SoundPickState};
use bevy::{
    audio::{AudioSink, AudioSinkPlayback, PlaybackSettings, Volume},
    prelude::*,
};

#[derive(Resource, Default)]
pub(crate) struct ScriptMusicPlayback {
    epoch: u64,
    pending: Option<(String, Option<usize>)>,
}

#[derive(Component)]
pub(crate) struct ScriptMusicVoice {
    gain: f32,
    from: f32,
    start_ms: f64,
    end_ms: Option<f64>,
}

impl ScriptMusicVoice {
    fn envelope(&self, now: f64) -> f32 {
        match self.end_ms {
            None => 1.0,
            Some(end) if end <= self.start_ms => 0.0,
            Some(end) => {
                self.from
                    * (1.0 - ((now - self.start_ms) / (end - self.start_ms)).clamp(0.0, 1.0) as f32)
            }
        }
    }
    fn stop(&mut self, now: f64, fade_ms: i32) {
        self.from = self.envelope(now);
        self.start_ms = now;
        self.end_ms = Some(now + f64::from(fade_ms));
    }
}

pub(crate) fn update_script_music(
    mut commands: Commands,
    epoch: Res<crate::backend::MatchEpoch>,
    ready: Res<crate::AudioReady>,
    loading: Option<Res<assets::LoadingScreen>>,
    time: Res<Time<Real>>,
    mut events: MessageReader<net::SvcScriptAudio>,
    bank: Option<Res<SoundBank>>,
    mut clips: Option<ResMut<ClipStore>>,
    mut looping: ResMut<Assets<LoopingPcmAudio>>,
    mut pcm: ResMut<Assets<PcmAudio>>,
    mut playback: ResMut<ScriptMusicPlayback>,
    mut voices: Query<(
        Entity,
        &mut ScriptMusicVoice,
        &mut PlaybackSettings,
        Option<&mut AudioSink>,
    )>,
    mut gaps: ResMut<crate::MissingAliasGaps>,
    mut pick: ResMut<SoundPickState>,
    settings: Option<Res<frame::GameSettings>>,
    mut mix: ResMut<crate::script_mix::ScriptAudioMix>,
) {
    let now = time.elapsed_secs_f64() * 1000.0;
    mix.reset_epoch(epoch.0);
    let mut retired = Vec::new();
    if playback.epoch != epoch.0 {
        playback.epoch = epoch.0;
        playback.pending = None;
        for (entity, _, _, _) in &voices {
            commands.entity(entity).try_despawn();
            retired.push(entity);
        }
    }
    for (entity, voice, _, sink) in &voices {
        if (voice.end_ms.is_some_and(|end| now >= end)
            || sink.is_some_and(AudioSinkPlayback::empty))
            && !retired.contains(&entity)
        {
            commands.entity(entity).try_despawn();
            retired.push(entity);
        }
    }
    for event in events.read() {
        match &event.0 {
            sim::ScriptAudioCommand::ChannelVolumes { .. }
            | sim::ScriptAudioCommand::DeactivateChannelVolumes { .. } => {}
            sim::ScriptAudioCommand::MusicPlay(alias) => {
                let (namespace, name) = namespace_alias(alias);
                let variant = bank
                    .as_ref()
                    .and_then(|bank| bank.0.sound_in(namespace, name))
                    .and_then(|sound| pick_variant(sound, &mut pick, namespace, name));
                if bank.is_some() && variant.is_none() {
                    gaps.record(alias);
                    continue;
                }
                if playback.pending.is_some()
                    || voices.iter().any(|(e, _, _, _)| !retired.contains(&e))
                {
                    diag::warn!(
                        Audio,
                        "audio: music stream is busy; alias {alias} was not started"
                    );
                    continue;
                }
                playback.pending = Some((alias.clone(), variant));
            }
            sim::ScriptAudioCommand::SoundFade { volume, fade_ms } => {
                mix.fade(now, *volume, *fade_ms);
                if *volume == 0.0 && *fade_ms == 0 {
                    playback.pending = None;
                    for (entity, _, _, _) in &voices {
                        if !retired.contains(&entity) {
                            retired.push(entity);
                        }
                    }
                    crate::backend::stop_all_match(&mut commands, epoch.0);
                }
            }
            sim::ScriptAudioCommand::MusicStop { fade_ms } => {
                playback.pending = None;
                for (entity, mut voice, _, _) in &mut voices {
                    if retired.contains(&entity) {
                        continue;
                    }
                    voice.stop(now, *fade_ms);
                    if *fade_ms == 0 {
                        commands.entity(entity).try_despawn();
                        retired.push(entity);
                    }
                }
            }
        }
    }
    let master = settings.map_or(1.0, |s| s.master_volume);
    for (entity, voice, mut settings, sink) in &mut voices {
        if retired.contains(&entity) {
            continue;
        }
        settings.volume = Volume::Linear(voice.gain * voice.envelope(now) * master);
        if let Some(mut sink) = sink {
            sink.set_volume(settings.volume);
        }
    }
    if !ready.0 || loading.is_some_and(|screen| !screen.is_complete()) {
        return;
    }
    let (Some(bank), Some(clips), Some((alias, variant))) =
        (bank, clips.as_deref_mut(), playback.pending.as_mut())
    else {
        return;
    };
    let (namespace, name) = namespace_alias(alias);
    if variant.is_none() {
        *variant = bank
            .0
            .sound_in(namespace, name)
            .and_then(|sound| pick_variant(sound, &mut pick, namespace, name));
    }
    let Some(selected) = *variant else {
        gaps.record(alias);
        playback.pending = None;
        return;
    };
    let prepared = match prepare_background(alias, selected, &bank.0, clips, &mut pick, &mut gaps) {
        Ok(Some(prepared)) => prepared,
        Ok(None) => return,
        Err(()) => {
            playback.pending = None;
            return;
        }
    };
    let volume = Volume::Linear(prepared.gain * master);
    let entity = if prepared.looping {
        crate::backend::spawn_loop(
            &mut commands,
            looping.add(prepared.pcm.into_looping()),
            volume,
            epoch.0,
            crate::backend::AudioScope::Match,
        )
    } else {
        crate::backend::spawn_oneshot(
            &mut commands,
            pcm.add(prepared.pcm),
            volume,
            prepared.speed,
            epoch.0,
            crate::backend::AudioScope::Match,
        )
    };
    commands.entity(entity).insert((
        crate::backend::SoundChannel(prepared.channel),
        ScriptMusicVoice {
            gain: prepared.gain,
            from: 1.0,
            start_ms: now,
            end_ms: None,
        },
        PlaybackSettings::ONCE
            .with_volume(volume)
            .with_speed(prepared.speed),
    ));
    pick.last_variant
        .insert((namespace, name.to_owned()), selected);
    playback.pending = None;
}

pub(crate) fn register(app: &mut App) {
    app.init_resource::<ScriptMusicPlayback>()
        .add_systems(Update, update_script_music.in_set(net::ClientSet::Effects));
}
