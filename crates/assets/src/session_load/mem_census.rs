//! Fork-owned: the memory census's view of the process-wide match statics
//! (`RESIDENT_MAP`, `COMMON`). Destructive and exact: each part is dropped in
//! turn and the fall in the counting allocator's live heap is its size, for the
//! bytes nothing else still shares. Only `IW4L_MEM_CENSUS_DROP=1` calls this,
//! at the very end of a run.

use super::common_cache::COMMON;
use super::*;

fn live() -> i64 {
    diag::process_live_heap_bytes().unwrap_or(0) as i64
}

macro_rules! part {
    ($out:ident, $name:expr, $drop:expr) => {{
        let before = live();
        drop($drop);
        $out.push(($name.to_string(), before - live()));
    }};
}

macro_rules! parts {
    ($out:ident, $prefix:literal, $src:ident, $($f:ident),* $(,)?) => {$(
        part!($out, concat!($prefix, stringify!($f)), $src.$f);
    )*};
}

pub fn drop_statics(common_first: bool) -> Vec<(String, i64)> {
    let mut out = Vec::with_capacity(96);
    if common_first {
        drop_common(&mut out);
        drop_resident(&mut out);
    } else {
        drop_resident(&mut out);
        drop_common(&mut out);
    }
    out
}

fn drop_resident(out: &mut Vec<(String, i64)>) {
    let Some(m) = super::resident_map::take_resident() else {
        return;
    };
    let PreparedMatch {
        scripts,
        world,
        fx,
        materials,
        clip,
        weapons,
        fpv_meshes,
        bodies,
        world_weapons,
        projectile_meshes,
        xanims,
        player_anim_sources,
        tracers,
        strings,
        prepared_map,
        sound,
        ..
    } = m;
    let PreparedWorld {
        draw,
        static_model_meshes,
        map_xmodel_scene_assets,
        fx: world_fx,
        fx_models,
        reflection_probe_images,
        film_visions,
        light_grid,
        smodel_lighting_samples,
        ..
    } = world;
    part!(out, "resident.world.draw", draw);
    part!(
        out,
        "resident.world.static_model_meshes",
        static_model_meshes
    );
    part!(
        out,
        "resident.world.map_xmodel_scene_assets",
        map_xmodel_scene_assets
    );
    part!(out, "resident.world.fx", world_fx);
    part!(out, "resident.world.fx_models", fx_models);
    part!(
        out,
        "resident.world.reflection_probe_images",
        reflection_probe_images
    );
    part!(out, "resident.world.film_visions", film_visions);
    part!(out, "resident.world.light_grid", light_grid);
    part!(
        out,
        "resident.world.smodel_lighting_samples",
        smodel_lighting_samples
    );
    part!(out, "resident.scripts", scripts);
    part!(out, "resident.fx", fx);
    part!(
        out,
        "resident.materials (Arc MaterialDefinitions)",
        materials
    );
    part!(out, "resident.clip (Arc ClipCollision)", clip);
    part!(out, "resident.weapons", weapons);
    part!(out, "resident.fpv_meshes", fpv_meshes);
    part!(out, "resident.bodies", bodies);
    part!(out, "resident.world_weapons", world_weapons);
    part!(out, "resident.projectile_meshes", projectile_meshes);
    part!(out, "resident.xanims", xanims);
    part!(out, "resident.player_anim_sources", player_anim_sources);
    part!(out, "resident.tracers", tracers);
    part!(out, "resident.strings", strings);
    part!(out, "resident.prepared_map", prepared_map);
    part!(out, "resident.sound (SoundCatalog)", sound);
}

fn drop_common(out: &mut Vec<(String, i64)>) {
    let flight = COMMON.lock().unwrap_or_else(|p| p.into_inner()).take();
    let Some(set) = flight.as_ref().and_then(|f| f.set.get().cloned().flatten()) else {
        return;
    };
    drop(flight);
    let products = set
        .products
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take();
    if let Some(p) = products {
        parts!(
            out,
            "common.",
            p,
            scripts,
            t5_scene_models,
            material_seed,
            shared_surfaces,
            scene_models,
            light_defs,
            tracers,
            teamsets,
            film_visions,
            weapons,
            fpv_meshes,
            world_weapons,
            projectile_meshes,
            xanims,
            player_anim_sources,
            fx,
            fx_models,
            impact_fx,
            t5_xanims,
            t5_fx,
            t5_impact_fx,
            iw5_materials,
            iw5_scene_models,
            iw5_shared_surfaces,
            strings,
            report,
            localize_report,
        );
    }
    let retained = std::mem::take(&mut *set.retained.lock().unwrap_or_else(|p| p.into_inner()));
    part!(
        out,
        "common.retained (kept image payloads for the next map)",
        retained
    );
    part!(out, "common.set (donor images, cac tables, rest)", set);
}
