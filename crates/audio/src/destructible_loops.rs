//! The loops a destructible speaks while it sits in a stage: gas hissing out
//! of a punctured tank, a propane fire burning on the cap. The host publishes
//! which alias each prop is speaking and where; this reconciles that set
//! against the positional emitters that carry it.

use crate::ambient::{MapAmbient, MapEmitter, SoundBankNamespace};
use crate::clip_store::{ClipStore, clip_keys_for_alias};
use crate::pcm::PcmAudio;
use crate::playback::{MissingAliasGaps, SharedPlayAssets, SoundBank};
use asset_core::AssetNamespace;
use bevy::prelude::*;
use net::PresentedSnapshot;
use sim::DestructibleLoopSound;
use std::sync::Arc;

#[derive(Component)]
pub(crate) struct DestructibleLoop {
    owner: u32,
    alias: String,
}

pub(crate) fn update(
    mut commands: Commands,
    presented: Res<PresentedSnapshot>,
    mut playing: Query<(
        Entity,
        &DestructibleLoop,
        Option<&mut crate::backend::SoundEntity>,
    )>,
    bank: Option<Res<SoundBank>>,
    namespace: Option<Res<SoundBankNamespace>>,
    mut clips: Option<ResMut<ClipStore>>,
    mut pcm: ResMut<Assets<PcmAudio>>,
    mut shared: ResMut<SharedPlayAssets>,
    mut gaps: ResMut<MissingAliasGaps>,
) {
    let Some(snapshot) = presented.snapshot() else {
        return;
    };
    let speaking: Vec<(&DestructibleLoopSound, &str)> = snapshot
        .meta
        .world_objects
        .destructible_loop_sounds
        .iter()
        .filter_map(|row| {
            let alias = snapshot
                .meta
                .sound_aliases
                .iter()
                .find(|(index, _)| *index == row.alias_index)?;
            Some((row, alias.1.as_str()))
        })
        .collect();

    for (entity, loop_sound, _) in &playing {
        if !speaking.iter().any(|(row, alias)| {
            row.owner.to_wire() == loop_sound.owner && *alias == loop_sound.alias
        }) {
            commands.entity(entity).try_despawn();
            diag::info!(
                Audio,
                "audio: destructible loop `{}` stopped",
                loop_sound.alias
            );
        }
    }

    let (Some(bank), Some(clips)) = (bank, clips.as_mut()) else {
        return;
    };
    let ns = namespace.map_or(AssetNamespace::Iw4, |map| map.namespace);
    for (row, alias) in speaking {
        if let Some((entity, _, current)) = playing
            .iter_mut()
            .find(|(_, playing, _)| playing.owner == row.owner.to_wire() && playing.alias == alias)
        {
            match (row.snd_ent, current) {
                (Some(number), Some(mut current)) => current.0 = number,
                (Some(number), None) => {
                    commands
                        .entity(entity)
                        .insert(crate::backend::SoundEntity(number));
                }
                (None, Some(_)) => {
                    commands
                        .entity(entity)
                        .remove::<crate::backend::SoundEntity>();
                }
                (None, None) => {}
            }
            continue;
        }
        let Some(key) = clip_keys_for_alias(&bank.0, ns, alias).into_iter().next() else {
            gaps.record(alias);
            continue;
        };
        clips.request(key.clone());
        let Some(Ok(audio)) = clips.ready(&key) else {
            continue;
        };
        let Some(sound) = crate::clip_store::alias_for_clip(&bank.0, ns, alias, &key) else {
            gaps.record(alias);
            continue;
        };
        let knots = sound
            .volume_falloff
            .as_ref()
            .map(|curve| shared.intern_curve(&curve.name, &curve.knots))
            .unwrap_or_else(|| Arc::from(Vec::<[f32; 2]>::new()));
        if knots.is_empty() {
            diag::warn!(
                Audio,
                "audio: destructible loop `{alias}` has no falloff curve (typed gap)"
            );
        }
        let emitter = commands
            .spawn((
                DestructibleLoop {
                    owner: row.owner.to_wire(),
                    alias: alias.to_owned(),
                },
                MapAmbient,
                MapEmitter {
                    origin_inches: row.origin,
                    dist_min: sound.dist_min,
                    dist_max: sound.dist_max,
                    knots,
                    base_gain: sound.vol_min.max(0.0),
                    pcm: pcm.add(audio),
                    live_pan: None,
                },
                Transform::from_translation(Vec3::from_array(row.origin)),
            ))
            .id();
        if let Some(flags) = sound.decoded_flags() {
            commands
                .entity(emitter)
                .insert(crate::backend::SoundChannel(flags.channel()));
        }
        if let Some(number) = row.snd_ent {
            commands
                .entity(emitter)
                .insert(crate::backend::SoundEntity(number));
        }
        diag::info!(Audio, "audio: destructible loop `{alias}`");
    }
}
