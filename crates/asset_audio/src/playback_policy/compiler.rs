use super::*;

struct CueSemantics {
    spatial: Option<Result<SpatialPlaybackPolicy, SpatialPolicyFailure>>,
    limits: [VoiceLimit; 2],
    group: GroupSelection,
    zero_volume_unity: bool,
    streamed_decode: crate::StreamedDecodePolicy,
    secondary: (SecondaryActivation, SecondaryPolicySource),
}

enum GroupSelection {
    Ungrouped,
    Index(u32),
    Unknown,
}

trait CueCompiler {
    fn prepare(&self, row: &CapturedAlias, channel: Option<&EntChannel>) -> CueSemantics;
}

struct IwCueCompiler {
    namespace: AssetNamespace,
}
struct T5CueCompiler;
struct T6CueCompiler;

impl CueCompiler for IwCueCompiler {
    fn prepare(&self, row: &CapturedAlias, channel: Option<&EntChannel>) -> CueSemantics {
        CueSemantics {
            spatial: match channel {
                Some(info) if info.is_3d => Some(native_curve(row)),
                Some(_) => None,
                None => Some(Err(SpatialPolicyFailure::MissingChannel(self.namespace))),
            },
            limits: [VoiceLimit::default(); 2],
            group: if self.namespace == AssetNamespace::Iw5 {
                row.vol_mod_index
                    .map_or(GroupSelection::Ungrouped, GroupSelection::Index)
            } else {
                GroupSelection::Ungrouped
            },
            zero_volume_unity: true,
            streamed_decode: crate::StreamedDecodePolicy::Detected,
            secondary: (
                SecondaryActivation::OnPrimaryPrepared,
                SecondaryPolicySource::PrimaryPreparedCompatibility,
            ),
        }
    }
}

impl CueCompiler for T5CueCompiler {
    fn prepare(&self, row: &CapturedAlias, _channel: Option<&EntChannel>) -> CueSemantics {
        CueSemantics {
            spatial: match row.flags {
                Some(flags) if flags & 2 != 0 => Some(native_curve(row)),
                Some(_) => None,
                None => Some(Err(SpatialPolicyFailure::Unsupported(AssetNamespace::T5))),
            },
            limits: [
                t5_limit(row, 25, row.limit_count, false),
                t5_limit(row, 27, row.entity_limit_count, true),
            ],
            group: row.flags.map_or(GroupSelection::Unknown, |flags| {
                GroupSelection::Index((flags >> 16) & 0x3f)
            }),
            zero_volume_unity: false,
            streamed_decode: crate::StreamedDecodePolicy::WmaContainerWithWaveCompatibility,
            secondary: (
                SecondaryActivation::OnResolution,
                SecondaryPolicySource::T5IndependentCompatibility,
            ),
        }
    }
}

impl CueCompiler for T6CueCompiler {
    fn prepare(&self, _row: &CapturedAlias, _channel: Option<&EntChannel>) -> CueSemantics {
        CueSemantics {
            spatial: Some(Err(SpatialPolicyFailure::Unsupported(AssetNamespace::T6))),
            limits: [VoiceLimit::default(); 2],
            group: GroupSelection::Ungrouped,
            zero_volume_unity: true,
            streamed_decode: crate::StreamedDecodePolicy::Detected,
            secondary: (
                SecondaryActivation::OnPrimaryPrepared,
                SecondaryPolicySource::PrimaryPreparedCompatibility,
            ),
        }
    }
}

fn native_curve(row: &CapturedAlias) -> Result<SpatialPlaybackPolicy, SpatialPolicyFailure> {
    let curve = row
        .volume_falloff
        .as_ref()
        .filter(|curve| !curve.knots.is_empty())
        .ok_or(SpatialPolicyFailure::MissingFalloffCurve)?;
    if !row.dist_min.is_finite()
        || !row.dist_max.is_finite()
        || curve
            .knots
            .iter()
            .chain(row.near_falloff.iter().flat_map(|near| near.knots.iter()))
            .any(|(x, y)| !x.is_finite() || !y.is_finite())
    {
        return Err(SpatialPolicyFailure::InvalidFalloffCurve);
    }
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
}

fn t5_limit(row: &CapturedAlias, shift: u32, count: Option<u8>, per_emitter: bool) -> VoiceLimit {
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
}

pub(super) fn compile(
    sound: &CapturedSound,
    variant: usize,
    row: &CapturedAlias,
    channels: Option<&[EntChannel]>,
    group_volumes: Option<&[Result<f32, crate::MixerGroupError>]>,
) -> AliasPlaybackPolicy {
    let namespace = AssetNamespace::from_zone_game(sound.game);
    let channel = sound
        .ent_channel(variant)
        .map(|id| ChannelKey { namespace, id });
    let channel_info = channel.and_then(|key| channels?.get(key.id as usize));
    let compiler: &dyn CueCompiler = match namespace {
        AssetNamespace::Iw4 | AssetNamespace::Iw5 => &IwCueCompiler { namespace },
        AssetNamespace::T5 => &T5CueCompiler,
        AssetNamespace::T6 => &T6CueCompiler,
    };
    let semantics = compiler.prepare(row, channel_info);
    let group_gain = match semantics.group {
        GroupSelection::Index(native_index) => {
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
        GroupSelection::Unknown => GroupGainPolicy::UnknownIndexUnityCompatibility,
        GroupSelection::Ungrouped => GroupGainPolicy::Ungrouped,
    };
    AliasPlaybackPolicy {
        namespace,
        looping: LoopingPolicy::compile(row.is_looping()),
        streamed_decode: semantics.streamed_decode,
        composition: CueCompositionPolicy::compile(
            namespace,
            row,
            semantics.secondary.0,
            semantics.secondary.1,
        ),
        stereo_speaker_gains: stereo_speaker_gains(namespace, row),
        loaded_binding_origin: row.loaded_binding_origin,
        channel,
        channel_admission: channel
            .zip(channel_info)
            .map(|(key, info)| ChannelAdmission {
                key,
                maximum: info.max_voices,
                restricted: info.is_restricted,
            }),
        priority: row.voice_priority.clone(),
        spatial: semantics.spatial,
        limits: semantics.limits,
        volume_range: if semantics.zero_volume_unity && row.vol_min == 0.0 && row.vol_max == 0.0 {
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

impl CueCompositionPolicy {
    fn compile(
        namespace: AssetNamespace,
        row: &CapturedAlias,
        activation: SecondaryActivation,
        source: SecondaryPolicySource,
    ) -> Self {
        let secondary = row
            .secondary
            .as_ref()
            .filter(|alias| !alias.is_empty())
            .map(|alias| SecondaryLayerPolicy {
                alias: alias.clone(),
                activation,
                source,
                lifetime: LayerLifetime::ParentGroup,
                pitch: LayerPitch::IndependentAuthoredRange,
                failure: LayerFailure::Independent,
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
        if row.stereo_speaker_gains.is_some() && stereo_speaker_gains(namespace, row).is_none() {
            unsupported.push(UnsupportedCueFeature::SpeakerGains);
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
