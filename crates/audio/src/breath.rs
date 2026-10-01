use asset_core::AssetNamespace;
use bevy::prelude::*;
use net::{LocalPresentClient, PresentedSnapshot};
use playerstate_iw4::{BREATH_HOLD_TIME_MS, weap_flags};

use crate::{AliasCommand, PlayAlias};

pub(crate) const IW_ALIASES: [&str; 4] = [
    "weap_sniper_breathin",
    "weap_sniper_breathout",
    "weap_sniper_breathgasp",
    "weap_sniper_heartbeat",
];
pub(crate) const T5_ALIASES: [&str; 4] = [
    "wpn_sniper_breathin",
    "wpn_sniper_breathout",
    "wpn_sniper_breathgasp",
    "wpn_sniper_heartbeat",
];

#[derive(Default)]
pub(crate) struct BreathAudio {
    active: bool,
    holding: bool,
    namespace: AssetNamespace,
    heartbeat_at: i32,
    heartbeat_started: bool,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn update(
    presented: Res<PresentedSnapshot>,
    local: Res<LocalPresentClient>,
    weapons: Option<Res<assets::PreparedWeapons>>,
    view: Option<Res<frame::ViewSubject>>,
    screen: Option<Res<frame::AppScreen>>,
    bank: Option<Res<crate::SoundBank>>,
    mut torn: MessageReader<frame::MatchTornDown>,
    mut died: MessageReader<frame::LifeEnded>,
    mut state: Local<BreathAudio>,
    mut play: MessageWriter<AliasCommand>,
) {
    let torn_down = torn.read().count() != 0;
    let died = died.read().any(|event| event.client == local.0.0);
    let ps = presented
        .alive_player(local.0)
        .filter(|_| !view.as_ref().is_some_and(|v| v.in_killcam()));
    if torn_down
        || died
        || !screen.is_some_and(|screen| matches!(*screen, frame::AppScreen::InGame))
        || ps.is_none()
    {
        if state.active {
            for alias in aliases(state.namespace) {
                play.write(AliasCommand::Stop {
                    namespace: state.namespace,
                    alias: alias.to_owned(),
                    snd_ent: Some(crate::SND_ENT_LOCAL),
                });
            }
        }
        *state = BreathAudio::default();
        return;
    }
    let holding = ps.is_some_and(|p| p.weap_flags & weap_flags::HOLD_BREATH != 0);
    let ns = ps
        .and_then(|ps| {
            weapons
                .as_ref()?
                .0
                .namespace_of(playerstate_iw4::get_viewmodel_weapon_index(ps))
        })
        .unwrap_or(AssetNamespace::Iw4);
    let aliases = aliases(ns);
    if state.active && ns != state.namespace {
        for alias in self::aliases(state.namespace) {
            play.write(AliasCommand::Stop {
                namespace: state.namespace,
                alias: alias.to_owned(),
                snd_ent: Some(crate::SND_ENT_LOCAL),
            });
        }
    }
    if state.holding && !holding && ns == state.namespace {
        let previous = self::aliases(state.namespace);
        for alias in [previous[0], previous[3]] {
            play.write(AliasCommand::Stop {
                namespace: state.namespace,
                alias: alias.to_owned(),
                snd_ent: Some(crate::SND_ENT_LOCAL),
            });
        }
        if let Some(ps) = ps {
            let alias = if ps.hold_breath_timer > BREATH_HOLD_TIME_MS {
                aliases[2]
            } else {
                aliases[1]
            };
            play.write(AliasCommand::Play(sound(ns, alias)));
        }
    }
    if holding && (!state.holding || ns != state.namespace) {
        play.write(AliasCommand::Play(sound(ns, aliases[0])));
        state.heartbeat_at = ps.map_or(0, |p| p.command_time).saturating_add(1000);
        state.heartbeat_started = false;
    }
    if holding
        && let Some(ps) = ps
        && ps.command_time >= state.heartbeat_at
    {
        let looping = bank
            .as_ref()
            .and_then(|bank| bank.0.sound_in(ns, aliases[3]))
            .is_some_and(|sound| {
                sound
                    .aliases
                    .iter()
                    .any(|alias| alias.decoded_flags().is_some_and(|flags| flags.looping()))
            });
        if !state.heartbeat_started || !looping {
            play.write(AliasCommand::Play(sound(ns, aliases[3])));
            state.heartbeat_started = true;
        }
        state.heartbeat_at = ps.command_time.saturating_add(1000);
    }
    state.active = true;
    state.holding = holding;
    state.namespace = ns;
}

pub(crate) fn aliases(namespace: AssetNamespace) -> [&'static str; 4] {
    if namespace == AssetNamespace::T5 {
        T5_ALIASES
    } else {
        IW_ALIASES
    }
}

fn sound(namespace: AssetNamespace, alias: &str) -> PlayAlias {
    PlayAlias {
        namespace,
        alias: alias.to_owned(),
        fallback: None,
        origin_inches: None,
        snd_ent: Some(crate::SND_ENT_LOCAL),
    }
}
