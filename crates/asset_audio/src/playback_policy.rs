use std::sync::Arc;

use crate::{AssetNamespace, CapturedAlias, CapturedSound, ChannelKey, EntChannel, VoicePriority};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VoiceLimitMode {
    #[default]
    Unlimited,
    Oldest,
    Reject,
    Priority,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VoiceLimitSource {
    #[default]
    NotAuthored,
    Native,
    UnknownFlagsUnlimitedCompatibility,
    ZeroOldestCountOneCompatibility,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct VoiceLimit {
    pub mode: VoiceLimitMode,
    pub count: u8,
    pub per_emitter: bool,
    pub source: VoiceLimitSource,
}

#[derive(Clone, Copy, Debug)]
pub struct ChannelAdmission {
    pub key: ChannelKey,
    pub maximum: i32,
    pub restricted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpatialPolicyFailure {
    Unsupported(AssetNamespace),
    MissingChannel(AssetNamespace),
    MissingFalloffCurve,
}

#[derive(Clone, Debug)]
pub struct SpatialPlaybackPolicy {
    pub dist_min: f32,
    pub dist_max: f32,
    pub knots: Arc<[[f32; 2]]>,
    pub near_knots: Option<Arc<[[f32; 2]]>>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScalarRangePolicy {
    Authored([f32; 2]),
    ZeroRangeUnityCompatibility,
}

impl ScalarRangePolicy {
    fn sample(self, random: f32) -> f32 {
        match self {
            Self::Authored([low, high]) => crate::lerp_range(low, high, random),
            Self::ZeroRangeUnityCompatibility => 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GroupGainPolicy {
    Ungrouped,
    Resolved {
        native_index: u32,
        scale: f32,
    },
    Invalid {
        native_index: u32,
        error: crate::MixerGroupError,
    },
    MissingGroupUnityCompatibility {
        native_index: u32,
    },
    UnknownIndexUnityCompatibility,
}

pub const MAX_SECONDARY_DEPTH: u8 = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecondaryActivation {
    OnResolution,
    OnPrimaryPrepared,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecondaryPolicySource {
    T5IndependentCompatibility,
    PrimaryPreparedCompatibility,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayerLifetime {
    ParentGroup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayerPitch {
    IndependentAuthoredRange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayerFailure {
    Independent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecondaryLayerPolicy {
    pub alias: String,
    pub activation: SecondaryActivation,
    pub source: SecondaryPolicySource,
    pub lifetime: LayerLifetime,
    pub pitch: LayerPitch,
    pub failure: LayerFailure,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnsupportedCueFeature {
    StartDelay(i32),
    Chain(String),
    MixerGroup(String),
    SpeakerMap(String),
}

#[derive(Clone, Debug)]
pub struct CueCompositionPolicy {
    pub secondary: Option<SecondaryLayerPolicy>,
    pub unsupported: Arc<[UnsupportedCueFeature]>,
}

impl CueCompositionPolicy {
    fn compile(namespace: AssetNamespace, row: &CapturedAlias) -> Self {
        let secondary = row
            .secondary
            .as_ref()
            .filter(|alias| !alias.is_empty())
            .map(|alias| {
                let (activation, source) = match namespace {
                    AssetNamespace::T5 => (
                        SecondaryActivation::OnResolution,
                        SecondaryPolicySource::T5IndependentCompatibility,
                    ),
                    _ => (
                        SecondaryActivation::OnPrimaryPrepared,
                        SecondaryPolicySource::PrimaryPreparedCompatibility,
                    ),
                };
                SecondaryLayerPolicy {
                    alias: alias.clone(),
                    activation,
                    source,
                    lifetime: LayerLifetime::ParentGroup,
                    pitch: LayerPitch::IndependentAuthoredRange,
                    failure: LayerFailure::Independent,
                }
            });
        let mut unsupported = Vec::new();
        if row.start_delay != 0 {
            unsupported.push(UnsupportedCueFeature::StartDelay(row.start_delay));
        }
        if let Some(name) = row.chain.as_ref().filter(|name| !name.is_empty()) {
            unsupported.push(UnsupportedCueFeature::Chain(name.clone()));
        }
        if !matches!(namespace, AssetNamespace::Iw4 | AssetNamespace::Iw5)
            && let Some(name) = row.mixer_group.as_ref().filter(|name| !name.is_empty())
        {
            unsupported.push(UnsupportedCueFeature::MixerGroup(name.clone()));
        }
        if stereo_speaker_gains(namespace, row).is_none()
            && let Some(name) = row.speaker_map.as_ref().filter(|name| !name.is_empty())
        {
            unsupported.push(UnsupportedCueFeature::SpeakerMap(name.clone()));
        }
        Self {
            secondary,
            unsupported: unsupported.into(),
        }
    }
}

fn stereo_speaker_gains(namespace: AssetNamespace, row: &CapturedAlias) -> Option<[[f32; 2]; 2]> {
    row.stereo_speaker_gains.filter(|gains| {
        matches!(namespace, AssetNamespace::Iw4 | AssetNamespace::Iw5)
            && gains
                .iter()
                .flatten()
                .all(|gain| gain.is_finite() && *gain >= 0.0)
    })
}

impl GroupGainPolicy {
    fn scale(self) -> Result<f32, crate::MixerGroupError> {
        match self {
            Self::Resolved { scale, .. } => Ok(scale),
            Self::Invalid { error, .. } => Err(error),
            _ => Ok(1.0),
        }
    }
}

#[derive(Clone, Debug)]
pub struct AliasPlaybackPolicy {
    pub namespace: AssetNamespace,
    pub authored_looping: Option<bool>,
    pub channel: Option<ChannelKey>,
    pub channel_admission: Option<ChannelAdmission>,
    pub priority: Option<VoicePriority>,
    pub spatial: Option<Result<SpatialPlaybackPolicy, SpatialPolicyFailure>>,
    pub limits: [VoiceLimit; 2],
    pub volume_range: ScalarRangePolicy,
    pub group_gain: GroupGainPolicy,
    pub pitch_range: ScalarRangePolicy,
    pub composition: CueCompositionPolicy,
    pub stereo_speaker_gains: Option<[[f32; 2]; 2]>,
    pub loaded_binding_origin: crate::LoadedBindingOrigin,
}

impl AliasPlaybackPolicy {
    pub fn volume(&self, random: f32) -> Result<f32, crate::MixerGroupError> {
        Ok(self.volume_range.sample(random) * self.group_gain.scale()?)
    }

    pub fn pitch(&self, random: f32) -> f32 {
        self.pitch_range.sample(random)
    }

    pub(crate) fn compile(
        sound: &CapturedSound,
        variant: usize,
        row: &CapturedAlias,
        channels: Option<&[EntChannel]>,
        group_volumes: Option<&[Result<f32, crate::MixerGroupError>]>,
    ) -> Self {
        let namespace = AssetNamespace::from_zone_game(sound.game);
        let channel = sound
            .ent_channel(variant)
            .map(|id| ChannelKey { namespace, id });
        let channel_info = channel.and_then(|key| channels?.get(key.id as usize));
        let native_curve = || {
            let curve = row
                .volume_falloff
                .as_ref()
                .filter(|curve| !curve.knots.is_empty())
                .ok_or(SpatialPolicyFailure::MissingFalloffCurve)?;
            let pack = |knots: &[(f32, f32)]| -> Arc<[[f32; 2]]> {
                knots
                    .iter()
                    .take(asset_iw4::SND_CURVE_MAX_KNOTS)
                    .map(|&(x, y)| [x, y])
                    .collect::<Vec<_>>()
                    .into()
            };
            Ok(SpatialPlaybackPolicy {
                dist_min: row.dist_min,
                dist_max: row.dist_max,
                knots: pack(&curve.knots),
                near_knots: row.near_falloff.as_ref().map(|curve| pack(&curve.knots)),
            })
        };
        let spatial = match namespace {
            AssetNamespace::T6 => Some(Err(SpatialPolicyFailure::Unsupported(namespace))),
            AssetNamespace::T5 => match row.flags {
                Some(flags) if flags & 2 != 0 => Some(native_curve()),
                Some(_) => None,
                None => Some(Err(SpatialPolicyFailure::Unsupported(namespace))),
            },
            AssetNamespace::Iw4 | AssetNamespace::Iw5 => match channel_info {
                Some(info) if info.is_3d => Some(native_curve()),
                Some(_) => None,
                None => Some(Err(SpatialPolicyFailure::MissingChannel(namespace))),
            },
        };
        let limit = |shift: u32, count: Option<u8>, per_emitter| {
            if namespace != AssetNamespace::T5 {
                return VoiceLimit::default();
            }
            let Some(count) = count else {
                return VoiceLimit::default();
            };
            let mode = match (row.flags.unwrap_or(0) >> shift) & 3u32 {
                1 => VoiceLimitMode::Oldest,
                2 => VoiceLimitMode::Reject,
                3 => VoiceLimitMode::Priority,
                _ => VoiceLimitMode::Unlimited,
            };
            VoiceLimit {
                mode,
                count: if mode == VoiceLimitMode::Oldest {
                    count.max(1)
                } else {
                    count
                },
                per_emitter,
                source: if row.flags.is_none() {
                    VoiceLimitSource::UnknownFlagsUnlimitedCompatibility
                } else if mode == VoiceLimitMode::Oldest && count == 0 {
                    VoiceLimitSource::ZeroOldestCountOneCompatibility
                } else {
                    VoiceLimitSource::Native
                },
            }
        };
        let group = match namespace {
            AssetNamespace::Iw5 => row.vol_mod_index,
            AssetNamespace::T5 => row.flags.map(|flags| (flags >> 16) & 0x3f),
            _ => None,
        };
        let group_gain = match group {
            Some(native_index) => {
                match group_volumes.and_then(|volumes| volumes.get(native_index as usize)) {
                    Some(&Ok(scale)) => GroupGainPolicy::Resolved {
                        native_index,
                        scale,
                    },
                    Some(&Err(error)) => GroupGainPolicy::Invalid {
                        native_index,
                        error,
                    },
                    None => GroupGainPolicy::MissingGroupUnityCompatibility { native_index },
                }
            }
            None if namespace == AssetNamespace::T5 => {
                GroupGainPolicy::UnknownIndexUnityCompatibility
            }
            None => GroupGainPolicy::Ungrouped,
        };
        Self {
            namespace,
            composition: CueCompositionPolicy::compile(namespace, row),
            stereo_speaker_gains: stereo_speaker_gains(namespace, row),
            loaded_binding_origin: row.loaded_binding_origin,
            authored_looping: row.is_looping(),
            channel,
            channel_admission: channel
                .zip(channel_info)
                .map(|(key, info)| ChannelAdmission {
                    key,
                    maximum: info.max_voices,
                    restricted: info.is_restricted,
                }),
            priority: row.voice_priority.clone(),
            spatial,
            limits: [
                limit(25, row.limit_count, false),
                limit(27, row.entity_limit_count, true),
            ],
            volume_range: if namespace != AssetNamespace::T5
                && row.vol_min == 0.0
                && row.vol_max == 0.0
            {
                ScalarRangePolicy::ZeroRangeUnityCompatibility
            } else {
                ScalarRangePolicy::Authored([row.vol_min, row.vol_max])
            },
            group_gain,
            pitch_range: if row.pitch_min == 0.0 && row.pitch_max == 0.0 {
                ScalarRangePolicy::ZeroRangeUnityCompatibility
            } else {
                ScalarRangePolicy::Authored([row.pitch_min, row.pitch_max])
            },
        }
    }
}
