use crate::backend::{AudioScope, MatchEpoch, SoundChannel, Voice};
use crate::pcm::{LiveGain, LoopingPcmAudio, PcmAudio};
use bevy::prelude::*;

#[derive(Resource)]
pub(crate) struct ScriptAudioMix {
    epoch: u64,
    pub(crate) gain: LiveGain,
    from: f32,
    to: f32,
    start_ms: f64,
    end_ms: f64,
}

impl Default for ScriptAudioMix {
    fn default() -> Self {
        Self {
            epoch: 0,
            gain: LiveGain::default(),
            from: 1.0,
            to: 1.0,
            start_ms: 0.0,
            end_ms: 0.0,
        }
    }
}

impl ScriptAudioMix {
    fn sample(&self, now: f64) -> f32 {
        if self.end_ms <= self.start_ms {
            return self.to;
        }
        let t = ((now - self.start_ms) / (self.end_ms - self.start_ms)).clamp(0.0, 1.0) as f32;
        self.from + (self.to - self.from) * t
    }
    pub(crate) fn fade(&mut self, now: f64, to: f32, duration: i32) {
        self.from = self.sample(now);
        self.to = to;
        self.start_ms = now;
        self.end_ms = now + f64::from(duration);
        self.gain.set(self.sample(now));
    }
    pub(crate) fn reset_epoch(&mut self, epoch: u64) {
        if self.epoch != epoch {
            self.epoch = epoch;
            self.from = 1.0;
            self.to = 1.0;
            self.start_ms = 0.0;
            self.end_ms = 0.0;
            self.gain.set(1.0);
        }
    }
}

struct ChannelGroup {
    active: bool,
    current: [f32; 64],
    goal: [f32; 64],
    rate: [f32; 64],
}

impl Default for ChannelGroup {
    fn default() -> Self {
        Self {
            active: false,
            current: [1.0; 64],
            goal: [1.0; 64],
            rate: [0.0; 64],
        }
    }
}

#[derive(Resource)]
pub(crate) struct ChannelAudioMix {
    epoch: u64,
    groups: [ChannelGroup; 4],
    selected: usize,
    gains: [LiveGain; 64],
    last_ms: f64,
    pending: std::collections::VecDeque<sim::ScriptAudioCommand>,
}

impl Default for ChannelAudioMix {
    fn default() -> Self {
        let mut groups = std::array::from_fn(|_| ChannelGroup::default());
        groups[0].active = true;
        Self {
            epoch: 0,
            groups,
            selected: 0,
            gains: std::array::from_fn(|_| LiveGain::default()),
            last_ms: 0.0,
            pending: Default::default(),
        }
    }
}

impl ChannelAudioMix {
    fn reset_epoch(&mut self, epoch: u64, now: f64) {
        if self.epoch != epoch {
            self.epoch = epoch;
            self.groups = std::array::from_fn(|_| ChannelGroup::default());
            self.groups[0].active = true;
            self.selected = 0;
            self.last_ms = now;
            self.pending.clear();
            for gain in &self.gains {
                gain.set(1.0);
            }
        }
    }
    fn advance(&mut self, now: f64) {
        let elapsed = (now - self.last_ms).max(0.0) as f32;
        self.last_ms = now;
        let group = &mut self.groups[self.selected];
        for i in 0..64 {
            let value = group.current[i] + group.rate[i] * elapsed;
            group.current[i] = if group.rate[i] < 0.0 {
                value.max(group.goal[i])
            } else {
                value.min(group.goal[i])
            };
            self.gains[i].set(group.current[i]);
        }
    }
    fn set(&mut self, now: f64, priority: u8, goals: &[f32], fade_ms: i32) {
        self.advance(now);
        let current = self.groups[self.selected].current;
        let group = &mut self.groups[usize::from(priority)];
        group.active = true;
        for (i, goal) in goals.iter().enumerate() {
            group.current[i] = current[i];
            group.goal[i] = *goal;
            group.rate[i] = (*goal - current[i]) / fade_ms.max(1) as f32;
        }
        self.selected = self
            .groups
            .iter()
            .rposition(|group| group.active)
            .unwrap_or(0);
        self.advance(now);
    }
    fn deactivate(&mut self, now: f64, priority: u8, fade_ms: i32) {
        self.advance(now);
        self.groups[usize::from(priority)].active = false;
        if self.selected == usize::from(priority) {
            let current = self.groups[self.selected].current;
            self.selected = self
                .groups
                .iter()
                .rposition(|group| group.active)
                .unwrap_or(0);
            let group = &mut self.groups[self.selected];
            group.current = current;
            for (i, value) in current.iter().enumerate() {
                group.rate[i] = (group.goal[i] - value) / fade_ms.max(1) as f32;
            }
        }
        self.advance(now);
    }
}

fn update_channel_mix(
    mut mix: ResMut<ChannelAudioMix>,
    mut events: MessageReader<net::SvcScriptAudio>,
    bank: Option<Res<crate::SoundBank>>,
    local: Option<Res<net::LocalPresentClient>>,
    epoch: Res<MatchEpoch>,
    time: Res<Time<Real>>,
) {
    let now = time.elapsed_secs_f64() * 1000.0;
    mix.reset_epoch(epoch.0, now);
    mix.advance(now);
    for event in events.read() {
        if event.0.target().is_some() {
            mix.pending.push_back(event.0.clone());
        }
    }
    let Some(local) = local else {
        return;
    };
    while let Some(command) = mix.pending.front() {
        if command.target() != Some(local.0) || !command.valid() {
            mix.pending.pop_front();
            continue;
        }
        if let sim::ScriptAudioCommand::ChannelVolumes { .. } = command
            && bank.is_none()
        {
            break;
        }
        let command = mix.pending.pop_front().expect("front command");
        match command {
            sim::ScriptAudioCommand::ChannelVolumes {
                priority,
                volumes,
                fade_ms,
                ..
            } => {
                let Some(bank) = bank.as_ref() else {
                    continue;
                };
                let goals: Option<Vec<_>> = match volumes {
                    Some(volumes) => bank
                        .0
                        .ent_channels
                        .iter()
                        .map(|channel| volumes.get(&channel.name.to_ascii_lowercase()).copied())
                        .collect(),
                    None => Some(vec![0.0; bank.0.ent_channels.len()]),
                };
                let Some(goals) = goals.filter(|goals| !goals.is_empty() && goals.len() <= 64)
                else {
                    diag::warn!(
                        Audio,
                        "audio: channel volume profile is incomplete for the sound bank"
                    );
                    continue;
                };
                mix.set(now, priority, &goals, fade_ms);
            }
            sim::ScriptAudioCommand::DeactivateChannelVolumes {
                priority, fade_ms, ..
            } => mix.deactivate(now, priority, fade_ms),
            _ => {}
        }
    }
}

fn bind_mix_gain(
    mut voices: Query<(
        &Voice,
        Option<&SoundChannel>,
        Option<&mut AudioPlayer<PcmAudio>>,
        Option<&mut AudioPlayer<LoopingPcmAudio>>,
    )>,
    mut pcm: ResMut<Assets<PcmAudio>>,
    mut looping: ResMut<Assets<LoopingPcmAudio>>,
    mut mix: ResMut<ScriptAudioMix>,
    mut channels: ResMut<ChannelAudioMix>,
    epoch: Res<MatchEpoch>,
    time: Res<Time<Real>>,
) {
    mix.reset_epoch(epoch.0);
    mix.gain.set(mix.sample(time.elapsed_secs_f64() * 1000.0));
    channels.reset_epoch(epoch.0, time.elapsed_secs_f64() * 1000.0);
    channels.advance(time.elapsed_secs_f64() * 1000.0);
    for (voice, channel, source, looped) in &mut voices {
        if voice.scope != AudioScope::Match || voice.epoch != epoch.0 {
            continue;
        }
        let channel_gain = channel.and_then(|channel| channels.gains.get(channel.0 as usize));
        if let Some(mut source) = source
            && let Some(audio) = pcm.get(&source.0)
            && (!audio.gain_bound_to(&mix.gain)
                || channel_gain.is_some_and(|gain| !audio.channel_gain_bound_to(gain)))
        {
            let mut bound = audio.with_gain(&mix.gain);
            if let Some(gain) = channel_gain {
                bound = bound.with_channel_gain(gain);
            }
            source.0 = pcm.add(bound);
        }
        if let Some(mut source) = looped
            && let Some(audio) = looping.get(&source.0)
            && (!audio.gain_bound_to(&mix.gain)
                || channel_gain.is_some_and(|gain| !audio.channel_gain_bound_to(gain)))
        {
            let mut bound = audio.with_gain(&mix.gain);
            if let Some(gain) = channel_gain {
                bound = bound.with_channel_gain(gain);
            }
            source.0 = looping.add(bound);
        }
    }
}

pub(crate) fn register(app: &mut App) {
    app.init_resource::<ScriptAudioMix>()
        .init_resource::<ChannelAudioMix>()
        .add_systems(Update, update_channel_mix.in_set(net::ClientSet::Effects))
        .add_systems(
            PostUpdate,
            (bind_mix_gain, crate::match_bus::route_match_voices)
                .chain()
                .before(bevy::transform::TransformSystems::Propagate),
        );
}
