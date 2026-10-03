//! Fork-owned behaviour of `iw4l serve` (the headless room host).
//!
//! Added only for `Role::Dedicated`. A listen host that loses its master goes
//! back to the menu; a dedicated one has no menu to go back to, so it exits
//! with a non-zero code and lets a supervisor restart it.

use std::time::Duration;

use bevy::{
    prelude::*,
    winit::{UpdateMode, WinitSettings},
};
use net::{MasterBridge, MasterBridgeState, MasterLaunchIntent};

/// Process exit code for "the master side is gone or was never configured".
pub const EXIT_BRIDGE_LOST: u8 = 2;

/// The authority runs in `FixedUpdate` and needs `Update` at 20 Hz or more.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

pub struct DedicatedPlugin;

impl Plugin for DedicatedPlugin {
    fn build(&self, app: &mut App) {
        let mode = UpdateMode::reactive(FRAME_INTERVAL);
        app.insert_resource(WinitSettings {
            focused_mode: mode,
            unfocused_mode: mode,
        })
        .add_systems(Update, exit_on_master_loss);
    }
}

/// Why the host should exit, if it should.
#[derive(Debug, PartialEq, Eq)]
enum ExitReason {
    NotHosting,
    BridgeFailed(String),
    BridgeClosed(String),
}

fn exit_decision(
    intent_enabled: bool,
    intent_is_join: bool,
    bridge: Option<&MasterBridgeState>,
) -> Option<ExitReason> {
    if !intent_enabled || intent_is_join {
        return Some(ExitReason::NotHosting);
    }
    match bridge? {
        MasterBridgeState::Failed { error, .. } => {
            Some(ExitReason::BridgeFailed(format!("{error:?}")))
        }
        MasterBridgeState::Closed { reason, .. } => {
            Some(ExitReason::BridgeClosed(format!("{reason:?}")))
        }
        _ => None,
    }
}

fn exit_on_master_loss(
    intent: Res<MasterLaunchIntent>,
    bridge: Option<Res<MasterBridge>>,
    mut exit: MessageWriter<AppExit>,
    mut done: Local<bool>,
) {
    if *done {
        return;
    }
    let state = bridge.map(|bridge| bridge.state());
    let Some(reason) = exit_decision(intent.enabled(), intent.is_join(), state.as_ref()) else {
        return;
    };
    match &reason {
        ExitReason::NotHosting => diag::error!(
            Net,
            "dedicated: no master host configured; set IW4L_MASTER_ADDR, IW4L_MASTER_SERVER_NAME and IW4L_MASTER_HOST_NAME. Exiting with code {EXIT_BRIDGE_LOST}"
        ),
        ExitReason::BridgeFailed(error) => diag::error!(
            Net,
            "dedicated: master bridge failed ({error}). Exiting with code {EXIT_BRIDGE_LOST}"
        ),
        ExitReason::BridgeClosed(why) => diag::error!(
            Net,
            "dedicated: master session closed ({why}). Exiting with code {EXIT_BRIDGE_LOST}"
        ),
    }
    *done = true;
    exit.write(AppExit::from_code(EXIT_BRIDGE_LOST));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exits_when_master_was_never_configured() {
        assert_eq!(
            exit_decision(false, false, None),
            Some(ExitReason::NotHosting)
        );
    }

    #[test]
    fn exits_when_intent_is_join() {
        assert_eq!(
            exit_decision(true, true, None),
            Some(ExitReason::NotHosting)
        );
    }

    #[test]
    fn exits_when_bridge_failed_or_closed() {
        let identity = net::SessionIdentity::unassigned(0);
        let failed = MasterBridgeState::Failed {
            identity,
            error: net::TransportFault::new("op", "host", "boom"),
        };
        assert!(matches!(
            exit_decision(true, false, Some(&failed)),
            Some(ExitReason::BridgeFailed(_))
        ));
        let closed = MasterBridgeState::Closed {
            identity,
            reason: master_protocol::SessionCloseReason::HostLeft,
        };
        assert!(matches!(
            exit_decision(true, false, Some(&closed)),
            Some(ExitReason::BridgeClosed(_))
        ));
    }

    #[test]
    fn keeps_running_while_connecting_or_hosting() {
        let connecting = MasterBridgeState::Connecting {
            identity: net::SessionIdentity::unassigned(0),
        };
        assert_eq!(exit_decision(true, false, Some(&connecting)), None);
    }

    #[test]
    fn keeps_running_before_the_bridge_exists() {
        assert_eq!(exit_decision(true, false, None), None);
    }
}
