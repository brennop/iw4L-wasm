use crate::{ClipStore, MissingAliasGaps, PcmAudio, SoundPickState};
use asset_core::AssetNamespace;

pub(crate) fn namespace_alias(alias: &str) -> (AssetNamespace, &str) {
    alias
        .split_once(':')
        .and_then(|(ns, name)| AssetNamespace::parse(ns).map(|ns| (ns, name)))
        .unwrap_or((AssetNamespace::Iw4, alias))
}

pub(crate) fn pick_variant(
    sound: &asset_audio::CapturedSound,
    pick: &mut SoundPickState,
    namespace: AssetNamespace,
    name: &str,
) -> Option<usize> {
    if sound.aliases.is_empty() {
        return None;
    }
    let weights: Vec<f32> = sound.aliases.iter().map(|row| row.probability).collect();
    Some(asset_iw4::pick_weighted_variant_index(
        &weights,
        &mut pick.lcg,
        pick.last_variant
            .get(&(namespace, name.to_owned()))
            .copied(),
    ))
}

pub(crate) struct PreparedBackground {
    pub pcm: PcmAudio,
    pub looping: bool,
    pub gain: f32,
    pub speed: f32,
    pub channel: u32,
}

pub(crate) fn prepare_background(
    alias: &str,
    variant: usize,
    bank: &asset_audio::SoundCatalog,
    clips: &mut ClipStore,
    pick: &mut SoundPickState,
    gaps: &mut MissingAliasGaps,
) -> Result<Option<PreparedBackground>, ()> {
    let (namespace, name) = namespace_alias(alias);
    let Some(sound) = bank.sound_in(namespace, name) else {
        gaps.record(alias);
        return Err(());
    };
    let Some(row) = sound.aliases.get(variant) else {
        gaps.record(alias);
        return Err(());
    };
    let Some(flags) = row.decoded_flags() else {
        gaps.record(alias);
        return Err(());
    };
    let key = row
        .loaded
        .bound_index()
        .map(crate::clip_store::ClipKey::Loaded)
        .or_else(|| {
            bank.streamed_for_variant(namespace, name, variant)
                .map(|(ns, dir, name)| crate::clip_store::ClipKey::Streamed { ns, dir, name })
        });
    let Some(key) = key else {
        gaps.record(alias);
        return Err(());
    };
    clips.request(key.clone());
    let Some(result) = clips.ready(&key) else {
        return Ok(None);
    };
    let Ok(sound) = result else {
        gaps.record(alias);
        return Err(());
    };
    let gain = asset_iw4::lerp_range(
        row.vol_min,
        row.vol_max,
        asset_iw4::unit_random(&mut pick.lcg),
    )
    .max(0.0);
    let speed = asset_iw4::lerp_range(
        row.pitch_min,
        row.pitch_max,
        asset_iw4::unit_random(&mut pick.lcg),
    );
    if !gain.is_finite() || !speed.is_finite() || speed <= 0.0 {
        gaps.record(alias);
        return Err(());
    }
    Ok(Some(PreparedBackground {
        pcm: sound,
        looping: flags.looping(),
        gain,
        speed,
        channel: flags.channel(),
    }))
}
