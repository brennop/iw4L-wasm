use std::collections::HashMap;

use bevy::prelude::*;
use frame::{MatchScope, ScopeEpoch};

use crate::backend::AudioScope;
use crate::sources::{DesiredSource, SourceCueRequest, SourceKey};
use crate::{AudioRuntime, SoundBank};

struct ProjectileLoop {
    weapon: u32,
    launch_time: i32,
    source: DesiredSource,
}

#[derive(Resource, Default)]
pub(crate) struct ProjectileSources {
    context: Option<(u64, u64)>,
    next_version: u64,
    live: HashMap<u32, ProjectileLoop>,
    pub desired: Vec<DesiredSource>,
}

pub(crate) fn update(
    presented: Res<net::PresentedSnapshot>,
    weapons: Option<Res<assets::PreparedWeapons>>,
    bank: Option<Res<SoundBank>>,
    clock: Option<Res<net::FrameClock>>,
    timeline: Res<net::EntityEventCursor>,
    epoch: Res<ScopeEpoch<MatchScope>>,
    runtime: Res<AudioRuntime>,
    mut sources: ResMut<ProjectileSources>,
) {
    let context = Some((epoch.0, timeline.timeline()));
    if sources.context != context {
        sources.context = context;
        sources.live.clear();
    }
    sources.desired.clear();
    let Some(bound) = weapons
        .as_ref()
        .and_then(|weapons| weapons.for_snapshot(presented.weapon_epoch()).ok())
    else {
        sources.live.clear();
        return;
    };
    let weapons = bound.registry();
    let at_time = clock
        .filter(|clock| clock.started())
        .map(|clock| clock.time())
        .unwrap_or_else(|| presented.tick().map(sim::level_time_ms).unwrap_or(0));
    sources.live.retain(|id, _| {
        presented
            .presented_projectiles()
            .iter()
            .any(|row| row.authoritative_id().is_some_and(|owner| owner.0 == *id))
    });
    for row in presented.presented_projectiles() {
        let net::PresentedProjectile::Authoritative(projectile) = row else {
            continue;
        };
        let id = projectile.id.0;
        let changed = sources.live.get(&id).is_none_or(|current| {
            current.weapon != projectile.weapon || current.launch_time != projectile.launch_time
        });
        if changed {
            sources.live.remove(&id);
            let Some(bank) = &bank else {
                continue;
            };
            let Some((namespace, alias)) = weapons.weapon_sound_key(
                projectile.weapon,
                asset_game::WeaponSoundSlot::Projectile,
                &bank.0,
            ) else {
                continue;
            };
            let Some(cue) = runtime.source_cue(SourceCueRequest {
                bank: bank.0.clone(),
                namespace,
                alias: alias.into(),
                emitter: u32::try_from(projectile.entnum).ok(),
                scope: AudioScope::Match,
                epoch: epoch.0,
                group: None,
            }) else {
                continue;
            };
            sources.next_version = sources
                .next_version
                .checked_add(1)
                .expect("source version exhausted");
            let version = sources.next_version;
            sources.live.insert(
                id,
                ProjectileLoop {
                    weapon: projectile.weapon,
                    launch_time: projectile.launch_time,
                    source: DesiredSource {
                        key: SourceKey {
                            scope: AudioScope::Match,
                            epoch: epoch.0,
                            object: u64::from(id),
                            slot: 7,
                        },
                        version,
                        cue,
                        origin_inches: None,
                        start_frame: runtime.audio_frame(),
                        gain: 1.0,
                        rate: 1.0,
                        audible: true,
                    },
                },
            );
        }
        let current = sources.live.get_mut(&id).expect("projectile source");
        current.source.origin_inches = Some(presented.projectile_origin_at(projectile, at_time));
        let desired = current.source.clone();
        sources.desired.push(desired);
    }
}
