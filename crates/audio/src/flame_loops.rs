use std::collections::HashMap;

use asset_game::WeaponSoundSlot;
use bevy::prelude::*;

use crate::backend::AudioScope;
use crate::sources::{DesiredSource, SourceCueRequest, SourceKey};
use crate::{AudioRuntime, FlameFire, SoundBank, WeaponSound};

const STREAM_GRACE_SECS: f32 = 0.25;

struct FlameStream {
    weapon: u32,
    player_view: bool,
    origin_inches: [f32; 3],
    last_fire: f32,
    source: Option<DesiredSource>,
}

#[derive(Resource, Default)]
pub(crate) struct FlameSources {
    context: Option<frame::PresentationContext>,
    next_version: u64,
    live: HashMap<u32, FlameStream>,
    pub desired: Vec<DesiredSource>,
}

fn slot_pair(
    player_view: bool,
    world: WeaponSoundSlot,
    player: WeaponSoundSlot,
) -> [WeaponSoundSlot; 2] {
    if player_view {
        [player, world]
    } else {
        [world, player]
    }
}

pub(crate) fn update(
    mut fires: MessageReader<FlameFire>,
    weapons: Option<Res<assets::PreparedWeapons>>,
    presented: Res<net::PresentedSnapshot>,
    bank: Option<Res<SoundBank>>,
    time: Res<Time>,
    timeline: Res<net::EntityEventCursor>,
    runtime: Res<AudioRuntime>,
    mut sounds: MessageWriter<WeaponSound>,
    mut sources: ResMut<FlameSources>,
    installed: Res<frame::InstalledMatch>,
) {
    let context = Some(frame::PresentationContext::new(
        installed.load_key,
        timeline.timeline(),
    ));
    if sources.context != context {
        sources.context = context;
        sources.live.clear();
    }
    sources.desired.clear();
    let (Some(bank), Some(bound)) = (
        bank,
        weapons
            .as_ref()
            .and_then(|weapons| weapons.for_snapshot(presented.weapon_epoch()).ok()),
    ) else {
        fires.clear();
        sources.live.clear();
        return;
    };
    let weapons = bound.registry();
    let key = |weapon: u32, slots: [WeaponSoundSlot; 2]| {
        slots
            .into_iter()
            .find_map(|slot| weapons.weapon_sound_key(weapon, slot, &bank.0))
    };
    let mut oneshot = |stream: &FlameStream, number: u32, world, player| {
        let Some((namespace, alias)) =
            key(stream.weapon, slot_pair(stream.player_view, world, player))
        else {
            return;
        };
        sounds.write(WeaponSound {
            event: None,
            namespace,
            alias: alias.to_owned(),
            origin_inches: (!stream.player_view).then_some(stream.origin_inches),
            snd_ent: Some(number),
        });
    };
    let now = time.elapsed_secs();
    for fire in fires.read() {
        let restarted = sources
            .live
            .get(&fire.number)
            .is_none_or(|stream| stream.weapon != fire.weapon);
        if restarted {
            let stream = FlameStream {
                weapon: fire.weapon,
                player_view: fire.player_view,
                origin_inches: fire.origin_inches,
                last_fire: now,
                source: None,
            };
            oneshot(
                &stream,
                fire.number,
                WeaponSoundSlot::FireStart,
                WeaponSoundSlot::FireStartPlayer,
            );
            sources.live.insert(fire.number, stream);
        }
        let stream = sources.live.get_mut(&fire.number).expect("flame stream");
        stream.player_view = fire.player_view;
        stream.origin_inches = fire.origin_inches;
        stream.last_fire = now;
    }
    let mut ended = Vec::new();
    for (&number, stream) in &sources.live {
        if now - stream.last_fire > STREAM_GRACE_SECS {
            ended.push(number);
        }
    }
    for number in ended {
        let stream = sources.live.remove(&number).expect("flame stream");
        oneshot(
            &stream,
            number,
            WeaponSoundSlot::FireStop,
            WeaponSoundSlot::FireStopPlayer,
        );
    }
    let FlameSources {
        next_version,
        live,
        desired,
        ..
    } = &mut *sources;
    for (&number, stream) in live.iter_mut() {
        if stream.source.is_none() {
            let Some((namespace, alias)) = key(
                stream.weapon,
                slot_pair(
                    stream.player_view,
                    WeaponSoundSlot::FireLoop,
                    WeaponSoundSlot::FireLoopPlayer,
                ),
            ) else {
                continue;
            };
            let Some(cue) = runtime.source_cue(SourceCueRequest {
                bank: bank.0.clone(),
                namespace,
                alias: alias.into(),
                emitter: Some(number),
                scope: AudioScope::Match,
                epoch: timeline.timeline(),
                group: None,
            }) else {
                continue;
            };
            *next_version = next_version
                .checked_add(1)
                .expect("source version exhausted");
            stream.source = Some(DesiredSource {
                key: SourceKey {
                    scope: AudioScope::Match,
                    epoch: timeline.timeline(),
                    object: u64::from(number),
                    slot: 8,
                },
                version: *next_version,
                cue,
                origin_inches: None,
                start_frame: runtime.audio_frame(),
                gain: 1.0,
                rate: 1.0,
                audible: true,
            });
        }
        let Some(source) = stream.source.as_mut() else {
            continue;
        };
        source.origin_inches = (!stream.player_view).then_some(stream.origin_inches);
        desired.push(source.clone());
    }
}
