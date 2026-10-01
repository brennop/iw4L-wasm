use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use bevy::prelude::*;
use master_protocol::{AdmissionFailure, AdvertId, ContentFlags, MemberId, SessionCloseReason};
use tokio_util::sync::CancellationToken;

use crate::transport::bootstrap::BootstrapLane;
use crate::transport::udp_session::RelayMailbox;

// Worker half (QUIC sessions, browser poller, the systems that drive them).
// wasm32 has no sockets or threads, so there the launch intent is always
// disabled and none of this exists.
#[cfg(online)]
mod online;
#[cfg(online)]
pub use online::register_master_bridge;

#[cfg(not(online))]
pub fn register_master_bridge(app: &mut App) {
    app.init_resource::<PendingMasterMenuAction>().add_systems(
        Update,
        crate::signon::drive_match_boundary
            .in_set(crate::ClientSet::Load)
            .after(frame::SessionSwapApplied),
    );
}

type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;

pub const CONTENT_IW4: u8 = 1 << 0;
pub const CONTENT_IW5: u8 = 1 << 1;
pub const CONTENT_T5: u8 = 1 << 2;

pub const fn content_inventory(iw4: bool, iw5: bool, t5: bool) -> ContentFlags {
    ContentFlags(
        (if iw4 { CONTENT_IW4 } else { 0 })
            | (if iw5 { CONTENT_IW5 } else { 0 })
            | (if t5 { CONTENT_T5 } else { 0 }),
    )
}

pub fn content_required_by_map(map: &str) -> Result<ContentFlags> {
    let namespace = map
        .split_once(':')
        .map_or("iw4", |(namespace, _)| namespace);
    Ok(ContentFlags(match namespace {
        "iw4" => CONTENT_IW4,
        "iw5" => CONTENT_IW5,
        "t5" => CONTENT_T5,
        other => return Err(format!("unknown content namespace `{other}` in map `{map}`").into()),
    }))
}

pub fn content_names(flags: ContentFlags) -> String {
    let mut names = Vec::new();
    if flags.0 & CONTENT_IW4 != 0 {
        names.push("iw4");
    }
    if flags.0 & CONTENT_IW5 != 0 {
        names.push("iw5");
    }
    if flags.0 & CONTENT_T5 != 0 {
        names.push("t5");
    }
    if flags.0 & !(CONTENT_IW4 | CONTENT_IW5 | CONTENT_T5) != 0 {
        names.push("unknown");
    }
    names.join(",")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionIdentity {
    pub attempt_id: u64,
    pub room_id: AdvertId,
    pub member_id: MemberId,
    pub epoch: u32,
}

impl SessionIdentity {
    pub const fn unassigned(attempt_id: u64) -> Self {
        Self {
            attempt_id,
            room_id: AdvertId([0; 16]),
            member_id: MemberId([0; 16]),
            epoch: 0,
        }
    }

    pub fn match_key(&self) -> frame::MatchKey {
        if self.epoch == 0 || self.room_id.0 == [0; 16] {
            frame::MatchKey::NONE
        } else {
            frame::MatchKey::new(self.room_id.0, self.epoch)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransportFault {
    pub operation: &'static str,
    pub role: &'static str,
    pub source: String,
    pub close_reason: Option<String>,
}

impl TransportFault {
    pub fn new(operation: &'static str, role: &'static str, source: impl Into<String>) -> Self {
        Self {
            operation,
            role,
            source: source.into(),
            close_reason: None,
        }
    }
}

impl fmt::Display for TransportFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}: {}", self.role, self.operation, self.source)?;
        if let Some(close) = &self.close_reason {
            write!(f, " (quic close: {close})")?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MasterLifecycleFact {
    MemberLeft {
        member_id: MemberId,
    },
    SessionClosed {
        reason: SessionCloseReason,
    },

    AdmissionFailed {
        member_id: MemberId,
        reason: AdmissionFailure,
    },
}

#[derive(Clone, Debug)]
#[cfg_attr(not(online), allow(dead_code))]
struct MasterTarget {
    address: String,
    server_name: String,
    ca_cert: Option<PathBuf>,
}

#[derive(Clone, Debug)]
#[cfg_attr(not(online), allow(dead_code))]
struct HostConfig {
    password: String,
    auto_start_map: bool,
    target: MasterTarget,
    name: String,
    map: String,
    mode: String,
    max_players: u8,
    requires: ContentFlags,
    have: ContentFlags,
}

#[derive(Clone, Debug)]
#[cfg_attr(not(online), allow(dead_code))]
struct JoinConfig {
    password: String,
    target: MasterTarget,
    advert_id: AdvertId,
    map: String,
    mode: String,
    have: ContentFlags,
}

#[derive(Clone, Debug)]
#[cfg_attr(not(online), allow(dead_code))]
struct BrowserConfig {
    target: MasterTarget,
    have: ContentFlags,
}

#[derive(Clone, Debug)]
#[cfg_attr(not(online), allow(dead_code))]
enum MasterLaunchMode {
    Disabled,
    Browser(BrowserConfig),
    Host(HostConfig),
    Join(JoinConfig),
}

#[derive(Resource, Clone, Debug)]
pub struct MasterLaunchIntent(MasterLaunchMode);

impl MasterLaunchIntent {
    #[cfg(not(online))]
    pub fn browser_from_env(_have: ContentFlags) -> Result<Self> {
        Ok(Self::disabled())
    }

    #[cfg(not(online))]
    pub fn from_env_for_map(
        _map: &str,
        _have: ContentFlags,
        _requires: ContentFlags,
    ) -> Result<Self> {
        Ok(Self::disabled())
    }

    #[cfg(online)]
    pub fn browser_from_env(have: ContentFlags) -> Result<Self> {
        let Ok(address) = std::env::var("IW4L_MASTER_ADDR") else {
            return Ok(Self::disabled());
        };
        let server_name = std::env::var("IW4L_MASTER_SERVER_NAME")
            .map_err(|_| "IW4L_MASTER_ADDR requires IW4L_MASTER_SERVER_NAME")?;
        Ok(Self(MasterLaunchMode::Browser(BrowserConfig {
            target: MasterTarget {
                address,
                server_name,
                ca_cert: std::env::var_os("IW4L_MASTER_CA_CERT").map(PathBuf::from),
            },
            have,
        })))
    }

    #[cfg(online)]
    pub fn from_env_for_map(map: &str, have: ContentFlags, requires: ContentFlags) -> Result<Self> {
        let Ok(address) = std::env::var("IW4L_MASTER_ADDR") else {
            return Ok(Self(MasterLaunchMode::Disabled));
        };
        let server_name = std::env::var("IW4L_MASTER_SERVER_NAME")
            .map_err(|_| "IW4L_MASTER_ADDR requires IW4L_MASTER_SERVER_NAME")?;
        let target = MasterTarget {
            address,
            server_name,
            ca_cert: std::env::var_os("IW4L_MASTER_CA_CERT").map(PathBuf::from),
        };
        let host = std::env::var("IW4L_MASTER_HOST_NAME").ok();
        let join = std::env::var("IW4L_MASTER_JOIN").ok();
        match (host, join) {
            (Some(name), None) if !name.trim().is_empty() => {
                Ok(Self(MasterLaunchMode::Host(HostConfig {
                    password: std::env::var("IW4L_MASTER_PASSWORD").unwrap_or_default(),
                    auto_start_map: true,
                    target,
                    name,
                    map: map.to_owned(),
                    mode: std::env::var("IW4L_GAMETYPE").unwrap_or_else(|_| "dm".into()),
                    max_players: parse_max_players()?,
                    requires,
                    have,
                })))
            }
            (None, Some(advert_id)) => Ok(Self(MasterLaunchMode::Join(JoinConfig {
                password: std::env::var("IW4L_MASTER_PASSWORD").unwrap_or_default(),
                target,
                advert_id: advert_id.parse()?,
                map: map.to_owned(),
                mode: std::env::var("IW4L_GAMETYPE").unwrap_or_else(|_| "dm".into()),
                have,
            }))),
            (Some(_), Some(_)) => {
                Err("set only one of IW4L_MASTER_HOST_NAME or IW4L_MASTER_JOIN".into())
            }
            _ => Err("IW4L_MASTER_ADDR requires IW4L_MASTER_HOST_NAME or IW4L_MASTER_JOIN".into()),
        }
    }

    pub const fn is_join(&self) -> bool {
        matches!(self.0, MasterLaunchMode::Join(_))
    }

    pub const fn enabled(&self) -> bool {
        !matches!(self.0, MasterLaunchMode::Disabled)
    }

    pub const fn disabled() -> Self {
        Self(MasterLaunchMode::Disabled)
    }

    pub const fn configured(&self) -> bool {
        !matches!(self.0, MasterLaunchMode::Disabled)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MasterAdvert {
    pub id: AdvertId,
    pub name: String,
    pub map: String,
    pub mode: String,
    pub players: u8,
    pub max_players: u8,
    pub locked: bool,
    pub in_match: bool,
    pub requires: ContentFlags,
    pub available: ContentFlags,
    pub password_protected: bool,
    pub missing: ContentFlags,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MasterBrowserSnapshot {
    pub generation: u64,
    pub loading: bool,
    pub adverts: Vec<MasterAdvert>,
    pub error: Option<String>,
    pub have: ContentFlags,
}

#[derive(Resource)]
pub struct MasterBrowser {
    state: Arc<Mutex<MasterBrowserSnapshot>>,
    refresh: Arc<AtomicU64>,
    cancel: CancellationToken,
    worker: Option<JoinHandle<()>>,
}

impl MasterBrowser {
    pub fn snapshot(&self) -> MasterBrowserSnapshot {
        self.state
            .lock()
            .expect("master browser state poisoned")
            .clone()
    }

    pub fn refresh(&self) {
        self.refresh.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for MasterBrowser {
    fn drop(&mut self) {
        self.cancel.cancel();
        let _ = self.worker.take();
    }
}

#[derive(Clone, Debug)]
pub enum MasterMenuAction {
    Refresh,
    SetPassword {
        password: String,
    },
    Host {
        password: String,
        map: String,
        mode: String,
    },
    Join {
        password: String,
        advert_id: AdvertId,
        map: String,
        mode: String,
    },
    UpdateLobby {
        map: String,
        mode: String,
    },
    StartMatch {
        map: String,
        mode: String,
    },
    VoteToSkip,
    LeaveLobby,
}

#[derive(Clone, Debug)]
#[cfg_attr(not(online), allow(dead_code))]
enum MasterBridgeCommand {
    SetPassword {
        password: String,
    },
    UpdateLobby {
        map: String,
        mode: String,
    },
    StartMatch {
        map: String,
        mode: String,
    },
    VoteToSkip,
    MapLoaded {
        epoch: u32,
        map: u64,
        weapons: u64,
        classes: u64,
    },
    HostWorldReady {
        epoch: u32,
        map: u64,
        weapons: u64,
        classes: u64,
        #[allow(dead_code)]
        load_key: frame::LocalLoadKey,
    },
    MatchEnded {
        match_key: frame::MatchKey,
    },
    AuthorityProgress,
    AdmitEnter {
        member_id: MemberId,
        epoch: u32,
        bootstrap_id: u32,
        connection_id: Option<u64>,
        client_id: u32,
    },
    Shutdown,
}

#[derive(Resource, Default)]
pub struct PendingMasterMenuAction(pub Option<MasterMenuAction>);

#[cfg(online)]
fn parse_max_players() -> Result<u8> {
    match std::env::var("IW4L_MASTER_MAX_PLAYERS") {
        Ok(raw) => {
            let value = raw.parse::<u8>()?;
            if !(2..=master_protocol::MAX_SESSION_MEMBERS).contains(&value) {
                return Err(format!("IW4L_MASTER_MAX_PLAYERS={value} must be 2..=18").into());
            }
            Ok(value)
        }
        Err(_) => Ok(master_protocol::MAX_SESSION_MEMBERS),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MasterBridgeState {
    Connecting {
        identity: SessionIdentity,
    },
    Hosting {
        identity: SessionIdentity,
        name: String,
        map: String,
        mode: String,
        members: Vec<MemberId>,
        member_names: std::collections::HashMap<MemberId, String>,
        max_players: u8,
        skip_votes: u8,
        in_match: bool,
    },
    Joining {
        identity: SessionIdentity,
    },
    Joined {
        identity: SessionIdentity,
        name: String,
        map: String,
        mode: String,
        members: Vec<MemberId>,
        member_names: std::collections::HashMap<MemberId, String>,
        max_players: u8,
        skip_votes: u8,
        in_match: bool,
    },
    Closed {
        identity: SessionIdentity,
        reason: SessionCloseReason,
    },
    Left {
        identity: SessionIdentity,
    },
    Failed {
        identity: SessionIdentity,
        error: TransportFault,
    },
}

impl MasterBridgeState {
    pub fn identity(&self) -> SessionIdentity {
        match *self {
            Self::Connecting { identity }
            | Self::Hosting { identity, .. }
            | Self::Joining { identity }
            | Self::Joined { identity, .. }
            | Self::Closed { identity, .. }
            | Self::Left { identity }
            | Self::Failed { identity, .. } => identity,
        }
    }

    pub fn map(&self) -> Option<&str> {
        match self {
            Self::Hosting { map, .. } | Self::Joined { map, .. } => Some(map),
            _ => None,
        }
    }

    pub fn mode(&self) -> Option<&str> {
        match self {
            Self::Hosting { mode, .. } | Self::Joined { mode, .. } => Some(mode),
            _ => None,
        }
    }

    pub fn members(&self) -> &[MemberId] {
        match self {
            Self::Hosting { members, .. } | Self::Joined { members, .. } => members,
            _ => &[],
        }
    }

    pub fn skip_votes(&self) -> u8 {
        match *self {
            Self::Hosting { skip_votes, .. } | Self::Joined { skip_votes, .. } => skip_votes,
            _ => 0,
        }
    }

    pub fn in_match(&self) -> bool {
        match *self {
            Self::Hosting { in_match, .. } | Self::Joined { in_match, .. } => in_match,
            _ => false,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Failed { .. } | Self::Closed { .. } | Self::Left { .. }
        )
    }
}

#[derive(Resource)]
pub struct MasterBridge {
    state: Arc<Mutex<MasterBridgeState>>,
    commands: Arc<Mutex<Vec<MasterBridgeCommand>>>,
    installed_load: Arc<Mutex<Option<frame::LocalLoadKey>>>,
    mailbox: RelayMailbox,
    #[cfg_attr(not(online), allow(dead_code))]
    bootstrap: Arc<BootstrapLane>,
    close: CancellationToken,
    facts: Arc<Mutex<Vec<MasterLifecycleFact>>>,
    incarnation: u64,
    worker: Option<JoinHandle<()>>,
}

impl MasterBridge {
    pub fn state(&self) -> MasterBridgeState {
        self.state.lock().expect("master state poisoned").clone()
    }

    pub fn fail(&self, reason: &str) {
        publish_failed(
            &self.state,
            self.state().identity(),
            TransportFault::new("gameplay", "local", reason),
        );
        self.request_close();
    }

    fn request_close(&self) {
        self.send(MasterBridgeCommand::Shutdown);
        self.close.cancel();
    }

    pub fn mailbox(&self) -> RelayMailbox {
        self.mailbox.clone()
    }

    fn send(&self, command: MasterBridgeCommand) {
        self.commands
            .lock()
            .expect("master command queue poisoned")
            .push(command);
    }

    pub fn set_installed_load(&self, load: Option<frame::LocalLoadKey>) {
        *self.installed_load.lock().expect("installed load poisoned") = load;
    }

    pub fn report_map_loaded(&self, epoch: u32, descriptor: crate::MatchDescriptor) {
        self.send(MasterBridgeCommand::MapLoaded {
            epoch,
            map: descriptor.map,
            weapons: descriptor.weapons,
            classes: descriptor.classes,
        });
    }

    pub fn report_host_world_ready(
        &self,
        epoch: u32,
        descriptor: crate::MatchDescriptor,
        load_key: frame::LocalLoadKey,
    ) {
        self.send(MasterBridgeCommand::HostWorldReady {
            epoch,
            map: descriptor.map,
            weapons: descriptor.weapons,
            classes: descriptor.classes,
            load_key,
        });
    }

    pub fn report_authority_progress(&self) {
        self.send(MasterBridgeCommand::AuthorityProgress);
    }

    pub fn report_match_ended(&self, match_key: frame::MatchKey) {
        self.send(MasterBridgeCommand::MatchEnded { match_key });
    }

    pub fn start_hosted_match(&self, map: String, mode: String) {
        self.send(MasterBridgeCommand::StartMatch { map, mode });
    }

    pub fn leave(&self) {
        self.request_close();
    }

    pub fn admit_enter(
        &self,
        member_id: MemberId,
        epoch: u32,
        bootstrap_id: u32,
        connection_id: Option<u64>,
        client_id: u32,
    ) {
        self.send(MasterBridgeCommand::AdmitEnter {
            member_id,
            epoch,
            bootstrap_id,
            connection_id,
            client_id,
        });
    }

    pub fn incarnation(&self) -> u64 {
        self.incarnation
    }

    pub fn drain_facts(&self) -> Vec<MasterLifecycleFact> {
        std::mem::take(&mut *self.facts.lock().expect("master facts poisoned"))
    }
}

impl Drop for MasterBridge {
    fn drop(&mut self) {
        if let Ok(mut queue) = self.commands.lock() {
            queue.push(MasterBridgeCommand::Shutdown);
        }
        self.close.cancel();
        let _ = self.worker.take();
    }
}

#[derive(Resource, Default)]
pub struct MasterMatchStart(Option<MasterMatchOffer>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MasterMatchOffer {
    pub map: String,
    pub mode: String,
    pub match_key: frame::MatchKey,
}

impl MasterMatchStart {
    pub fn take(&mut self) -> Option<MasterMatchOffer> {
        self.0.take()
    }
}

fn publish_failed(
    state: &Mutex<MasterBridgeState>,
    fallback: SessionIdentity,
    error: TransportFault,
) {
    let mut current = state.lock().expect("master state poisoned");
    if current.is_terminal() {
        return;
    }
    let identity = current.identity();
    let identity = if identity.attempt_id == 0 {
        fallback
    } else {
        identity
    };
    diag::warn!(Net, "master relay failed: {error}");
    *current = MasterBridgeState::Failed { identity, error };
}
