//! Per-holder numbers.
//!
//! Two kinds, both from public items only:
//!
//! * `run`: non-destructive estimates (`capacity * size_of`), taken at every
//!   snapshot.
//! * `drop_probe`: destructive and exact. At the end of a run each resource is
//!   removed in turn and the drop in the counting allocator's live heap is
//!   recorded: the bytes that were held *only* by that resource. Bytes shared
//!   through an `Arc` are billed to whichever holder is dropped last, so run
//!   it again with `IW4L_MEM_CENSUS_DROP_ORDER=reverse` to see which are shared.
//!   The host is done afterwards (it exits).

use bevy::ecs::component::ComponentId;
use bevy::prelude::*;
use serde_json::{Value, json};

use super::Cats;

pub fn run(world: &World, cats: &mut Cats) {
    let _ = world;
    let zones = asset_transport::live_zone_images();
    let bytes = zones.iter().map(|z| z.1).sum();
    cats.add(
        "Zone images (fastfile bytes, live Arc<ZoneImage>)",
        zones.len(),
        bytes,
        true,
    );
    let payloads = asset_material::material_images::unapplied_decoded_bytes();
    cats.add(
        "Decoded image payloads alive (asset_material counter; includes the common set's kept payloads)",
        0,
        payloads as usize,
        true,
    );
}

/// Resources (name, shallow size) and entity/component counts, for finding holders.
pub fn discovery(world: &World) -> Value {
    let mut comps: std::collections::HashMap<String, (usize, usize)> = Default::default();
    for arch in world.archetypes().iter() {
        let n = arch.len() as usize;
        if n == 0 {
            continue;
        }
        for id in arch.components() {
            if let Some(info) = world.components().get_info(*id) {
                let e = comps.entry(info.name().to_string()).or_default();
                e.0 += n;
                e.1 = info.layout().size();
            }
        }
    }
    let mut comps: Vec<_> = comps.into_iter().collect();
    comps.sort_by_key(|(_, (n, sz))| std::cmp::Reverse(n * sz));
    comps.truncate(40);
    json!({
        "entities": world.entities().len(),
        "resources": world.iter_resources().count(),
        "top_components": comps.iter().map(|(n, (c, s))| json!([n, c, s])).collect::<Vec<_>>(),
    })
}

fn live() -> i64 {
    diag::process_live_heap_bytes().unwrap_or(0) as i64
}

/// Resources we must not drop: bevy/winit/wgpu internals other than the two
/// asset stores, and this census itself.
fn skip(name: &str) -> bool {
    if name.contains("mem_census") {
        return true;
    }
    if name.contains("bevy_asset::assets::Assets<") {
        return false;
    }
    name.starts_with("bevy_")
        || name.starts_with("wgpu")
        || name.starts_with("winit")
        || name.starts_with("gilrs")
        || name.starts_with("async")
}

macro_rules! take_fields {
    ($scene:expr, $out:expr, $($f:ident),* $(,)?) => {$(
        let before = live();
        drop(std::mem::take(&mut $scene.$f));
        $out.push((concat!("WorldScene.", stringify!($f)), before - live()));
    )*};
}

pub fn drop_probe(world: &mut World, reverse: bool) -> Value {
    // Everything allocated by the probe itself happens before the first read.
    let mut targets: Vec<(ComponentId, String)> = world
        .iter_resources()
        .filter_map(|(info, _)| {
            let name = info.name().to_string();
            (!skip(&name)).then(|| (info.id(), name))
        })
        .collect();
    targets.sort_by(|a, b| a.1.cmp(&b.1));
    if reverse {
        targets.reverse();
    }
    let mut fields: Vec<(&'static str, i64)> = Vec::with_capacity(64);
    let mut deltas: Vec<i64> = Vec::with_capacity(targets.len());
    let start = live();

    // WorldScene, field by field, before the rest of it goes.
    if let Some(mut scene) =
        world.get_resource_mut::<render_frontend::prepare::scene::world::WorldScene>()
    {
        take_fields!(
            scene,
            fields,
            sky_model,
            batches,
            exact_material_images,
            exact_material_names,
            lightmaps,
            reflection_probes,
            static_model_meshes,
            smodel_mesh_names,
            smodel_mark_cpu,
            map_xmodel_scene_assets,
            script_model_instances,
            dyn_ent_instances,
            dyn_ent_brushes,
            smodel_lighting_samples,
            static_model_instances,
            light_grid,
            cull,
            fx_glass,
            retained_packed_vertices,
            retained_vertex_layer,
            surface_vertex_layer,
            surface_first_vertex,
            retained_positions,
            retained_normals,
            retained_tangents,
            retained_colors,
            retained_texture_uvs,
            retained_lightmap_uvs,
            film_visions,
            light_region_hulls,
            shadow_geometry,
            primary_light_cull,
            primary_light_pack,
        );
    }

    let mut statics: Vec<(String, i64)> = Vec::new();
    if reverse {
        statics = assets::census_drop_statics(true);
    }
    let zones = asset_transport::live_zone_images();
    let mut total = 0i64;
    for (id, _) in &targets {
        let before = live();
        world.remove_resource_by_id(*id);
        let d = before - live();
        total += d;
        deltas.push(d);
    }
    let after_resources = live();
    if !reverse {
        statics = assets::census_drop_statics(false);
    }

    let before = live();
    world.clear_entities();
    let entities_freed = before - live();
    let after_entities = live();

    let mut rows: Vec<(String, i64)> = targets
        .into_iter()
        .zip(deltas)
        .map(|((_, name), d)| (name, d))
        .collect();
    rows.sort_by_key(|(_, d)| std::cmp::Reverse(*d));
    let small: i64 = rows.iter().skip(120).map(|(_, d)| *d).sum();
    rows.truncate(120);
    let sc: i64 = fields.iter().map(|(_, d)| *d).sum();
    json!({
        "order": if reverse { "reverse" } else { "forward" },
        "live_before": start,
        "resources_freed": total,
        "resources_freed_beyond_top120": small,
        "worldscene_fields_freed": sc,
        "entities_freed": entities_freed,
        "live_after_resources": after_resources,
        "live_after_entities": after_entities,
        "statics": statics.iter().map(|(n, d)| json!([n, d])).collect::<Vec<_>>(),
        "live_zone_images": zones.iter().map(|(p, b)| json!([p.display().to_string(), b])).collect::<Vec<_>>(),
        "worldscene_fields": fields.iter().map(|(n, d)| json!([n, d])).collect::<Vec<_>>(),
        "resources_top": rows.iter().map(|(n, d)| json!([n, d])).collect::<Vec<_>>(),
    })
}
