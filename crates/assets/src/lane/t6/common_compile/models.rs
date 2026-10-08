use crate::lane::CommonDependencyRefusal;
use asset_game::WeaponBuild;
use asset_material::MaterialCatalog;
use asset_model::{FpvMeshBuild, WorldWeaponBuild};
use std::sync::Arc;

const T6_RUNTIME_DECALS: [&str; 2] = ["mc/mtl_clan_tag", "mc/mtl_player_icon"];

pub(super) fn bind_t6_content(
    content: super::super::T6Content,
    weapons: &WeaponBuild,
    materials: &mut MaterialCatalog,
    fpv: &mut FpvMeshBuild,
    world: &mut WorldWeaponBuild,
    refusals: &mut Vec<CommonDependencyRefusal>,
) -> String {
    use asset_core::AssetNamespace::Iw4;
    let flat_normal = Arc::new(asset_material::solid_texture([128, 128, 255, 128], false));
    let neutral_specular = Arc::new(asset_material::solid_texture([48, 48, 48, 160], true));
    let mut bound: std::collections::HashMap<(String, bool), usize> = Default::default();
    let (mut views, mut worlds, mut no_donor) = (0usize, 0usize, 0usize);
    let mut donors_seen = std::collections::HashSet::new();
    let mut linked_techsets = std::collections::BTreeSet::new();
    let mut native_report = Vec::new();
    let mut native_n = 0usize;
    let mut donor_lines = Vec::new();
    for mut model in content.models {
        let donor = weapons
            .resolve_index(model.stand_in)
            .ok()
            .flatten()
            .and_then(|id| {
                let keys = if model.hands {
                    &fpv.get(Iw4, weapons.hand_xmodel_of(id)?)?.material_keys
                } else if model.view {
                    &fpv.get(Iw4, weapons.gun_xmodel_of(id)?)?.material_keys
                } else {
                    match weapons
                        .world_model_of(id)
                        .and_then(|name| world.get(Iw4, name))
                    {
                        Some(gun) => &gun.material_keys,
                        None => &fpv.get(Iw4, weapons.gun_xmodel_of(id)?)?.material_keys,
                    }
                };
                let lit_bodies: Vec<usize> = keys
                    .iter()
                    .flatten()
                    .filter_map(|key| {
                        materials.materials.iter().position(|m| {
                            m.namespace == key.namespace
                                && m.name.as_str() == asset_core::AssetRef::bare_name(&key.name)
                        })
                    })
                    .filter(|&index| {
                        let textures = &materials.materials[index].textures;
                        let has = |semantic| {
                            textures
                                .iter()
                                .any(|t| t.semantic == semantic && t.image.is_some())
                        };
                        has(asset_material::TS_COLOR_MAP) && has(asset_material::TS_NORMAL_MAP)
                    })
                    .collect();
                lit_bodies
                    .into_iter()
                    .min_by_key(|&index| materials.materials[index].sort_key)
            });
        if let Some(donor) = donor
            && donors_seen.insert(donor)
            && donors_seen.len() <= 4
        {
            let m = &materials.materials[donor];
            donor_lines.push(format!(
                "{}→{} textures={:?}",
                model.stand_in,
                m.name.as_str(),
                m.textures.iter().map(|t| t.semantic).collect::<Vec<_>>()
            ));
        }
        let Some(donor) = donor else {
            no_donor += 1;
            refusals.push(CommonDependencyRefusal::ModelDonor {
                model: model.skel.name.clone(),
                stand_in: model.stand_in,
                fields: asset_material::t6_techset::T6MaterialFields::ALL,
            });
            continue;
        };
        model.skel.surface_materials = model
            .surface_materials
            .iter()
            .map(|name| {
                let name = name.as_ref()?;
                if T6_RUNTIME_DECALS.contains(&name.as_str()) {
                    return None;
                }
                let key = (name.clone(), model.view);
                if let Some(&index) = bound.get(&key) {
                    return Some(asset_core::WalkLocalMaterialIndex::from_walk(index));
                }
                let captured = content.materials.get(name)?;
                if let Some(&index) = bound.get(&(name.clone(), true))
                    && captured.native.is_some()
                {
                    bound.insert(key, index);
                    return Some(asset_core::WalkLocalMaterialIndex::from_walk(index));
                }
                if let Some(native) = &captured.native
                    && let Some(set) = content.techsets.get(&native.technique_set)
                {
                    let draw = if native
                        .state
                        .first_bits(asset_material::t6_techset::T6_TECHNIQUE_LIT)
                        .is_none()
                        && native
                            .state
                            .first_bits(asset_material::t6_techset::T6_TECHNIQUE_EMISSIVE)
                            .is_some()
                    {
                        asset_material::t6_techset::T6Draw::Emissive
                    } else {
                        asset_material::t6_techset::T6Draw::Lit
                    };
                    let linked = draw.technique_set_name(&native.technique_set);
                    if !linked_techsets.contains(&linked) {
                        materials.link_t6_technique_set(set, draw, &mut native_report);
                        linked_techsets.insert(linked);
                    }
                    if let Ok(index) = materials.t6_material(
                        asset_material::t6_techset::T6MaterialSource::RequiresDonor(donor),
                        name,
                        set,
                        &native.textures,
                        native.constants.clone(),
                        &native.state,
                        draw,
                        &mut native_report,
                    ) {
                        native_n += 1;
                        bound.insert((name.clone(), true), index);
                        bound.insert(key, index);
                        return Some(asset_core::WalkLocalMaterialIndex::from_walk(index));
                    }
                }
                let textures = asset_material::StandInTextures {
                    color: captured
                        .color
                        .clone()
                        .map(|(image, texture)| (image, texture, true)),
                    normal: Some(captured.normal.clone().map_or_else(
                        || ("$t6_flat_normal".to_owned(), flat_normal.clone(), false),
                        |(image, texture)| (image, texture, false),
                    )),
                    specular: Some(captured.specular.clone().map_or_else(
                        || {
                            (
                                "$t6_neutral_specular".to_owned(),
                                neutral_specular.clone(),
                                true,
                            )
                        },
                        |(image, texture)| (image, texture, true),
                    )),
                };
                let stand_in_name = if model.view {
                    name.clone()
                } else {
                    format!("{name}#world")
                };
                let index = materials
                    .t6_donor_surface(
                        donor,
                        asset_core::MaterialKey {
                            namespace: asset_core::AssetNamespace::T6,
                            name: name.clone(),
                        },
                        &stand_in_name,
                        textures,
                    )
                    .ok()?;
                bound.insert(key, index);
                Some(asset_core::WalkLocalMaterialIndex::from_walk(index))
            })
            .collect();
        if model.view {
            fpv.insert_in(Iw4, model.skel, Some(materials));
            views += 1;
        } else {
            world.insert_in(Iw4, model.skel, Some(materials));
            worlds += 1;
        }
    }
    materials.resolve_technique_set_edges();
    format!(
        "t6 content bound: {views} first-person and {worlds} world models, {} materials ({native_n} with their own technique sets: {linked_techsets:?}); {no_donor} models without an IW4 donor material; donors e.g. {donor_lines:?}; {native_report:?}",
        bound.len()
    )
}
