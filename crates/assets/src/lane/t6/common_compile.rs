use std::sync::Arc;

use asset_game::WeaponBuild;
use asset_material::MaterialCatalog;
use asset_model::{FpvMeshBuild, WorldWeaponBuild};

use crate::lane::{
    CommonDependencyRefusal, CommonFamilyCompiler, CommonPreparationProducts,
    CommonPreparationResult,
};

pub(super) struct T6CommonCompiler {
    pub content: super::T6Content,
    pub captured_weapons: usize,
}

impl CommonFamilyCompiler for T6CommonCompiler {
    fn compile(self: Box<Self>, products: CommonPreparationProducts) -> CommonPreparationResult {
        let CommonPreparationProducts {
            mut weapons,
            materials: mut material_seed,
            fpv: mut fpv_meshes,
            world: mut world_weapons,
            projectiles: mut projectile_meshes,
            mut xanims,
            fx: mut common_fx,
            mut report,
        } = products;
        let t6_captured = self.captured_weapons;
        let mut content = self.content;
        let t6_sound_names = std::mem::take(&mut content.sound_names);
        let t6_hands = content.hands.take();
        let t6_melee = content.melee.take();
        let (added, kept) = xanims.absorb_vacant(std::mem::take(&mut content.xanims));
        report.push(format!(
            "t6 xanims absorbed: +{}, {kept} names already taken",
            added.len()
        ));
        let t6_anim_names: std::collections::BTreeSet<_> = added.into_iter().collect();
        let mut refusals = Vec::new();
        report.push(bind_t6_fx(
            std::mem::take(&mut content.fx),
            std::mem::take(&mut content.fx_materials),
            &mut material_seed,
            &mut common_fx,
            &mut refusals,
        ));
        report.push(bind_t6_content(
            content,
            &weapons,
            &mut material_seed,
            &mut fpv_meshes,
            &mut world_weapons,
            &mut refusals,
        ));
        let t6_dressed = weapons.dress_t6_stand_ins(
            |name| {
                fpv_meshes
                    .get(asset_core::AssetNamespace::Iw4, name)
                    .is_some()
            },
            |name| {
                world_weapons
                    .get(asset_core::AssetNamespace::Iw4, name)
                    .is_some()
            },
            |name| t6_sound_names.contains(name),
            |name| t6_anim_names.contains(name),
            t6_hands.as_deref().filter(|name| {
                fpv_meshes
                    .get(asset_core::AssetNamespace::Iw4, name)
                    .is_some()
            }),
            t6_melee.as_ref(),
        );
        let mut t6_projectiles = 0usize;
        for id in 1..weapons.len() as u32 {
            if weapons.identity_namespace_of(id) != Some(asset_core::AssetNamespace::T6) {
                continue;
            }
            let Some(name) = weapons.projectile_model_of(id) else {
                continue;
            };
            if !projectile_meshes.contains(asset_core::AssetNamespace::Iw4, name)
                && let Some(gun) = world_weapons.get(asset_core::AssetNamespace::Iw4, name)
            {
                projectile_meshes.absorb_world_weapon(gun);
                t6_projectiles += 1;
            }
        }
        report.push(format!(
        "t6 weapon absorb: captured={t6_captured} dressed={} own_view={} own_anims={} (hands {t6_hands:?}) dual_wield={} borrowed_melee={} own_world={} own_projectile={} (+{t6_projectiles} projectile meshes) own_sounds={} missing_stand_ins={:?}; registry now {} (t6={})",
        t6_dressed.dressed,
        t6_dressed.own_view,
        t6_dressed.own_anims,
        t6_dressed.dual_wield,
        t6_dressed.borrowed_melee,
        t6_dressed.own_world,
        t6_dressed.own_projectile,
        t6_dressed.own_sounds,
        t6_dressed.missing,
        weapons.len(),
        weapons.namespace_count(asset_core::AssetNamespace::T6)
    ));

        CommonPreparationResult {
            products: CommonPreparationProducts {
                weapons,
                materials: material_seed,
                fpv: fpv_meshes,
                world: world_weapons,
                projectiles: projectile_meshes,
                xanims,
                fx: common_fx,
                report,
            },
            refusals,
        }
    }
}

const T6_RUNTIME_DECALS: [&str; 2] = ["mc/mtl_clan_tag", "mc/mtl_player_icon"];

fn bind_t6_content(
    content: super::T6Content,
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
                    if let Some(index) = materials.t6_material(
                        donor,
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
                let index = materials.stand_in_material(donor, &stand_in_name, textures)?;
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

const T6_FX_DONOR_EFFECT: &str = "misc/glow_stick_glow_green";

fn bind_t6_fx(
    effects: Vec<asset_game::T6FxCapture>,
    fx_materials: std::collections::BTreeMap<String, super::T6MaterialCapture>,
    materials: &mut MaterialCatalog,
    catalog: &mut asset_game::FxCatalog,
    refusals: &mut Vec<CommonDependencyRefusal>,
) -> String {
    use asset_core::AssetNamespace::Iw4;
    let donor = catalog.get_in(Iw4, T6_FX_DONOR_EFFECT).and_then(|fx| {
        fx.elems
            .iter()
            .flat_map(|elem| elem.visuals.iter())
            .flat_map(|visual| visual.decode_keys())
            .find_map(|key| materials.material_index_by_ns(key.namespace, &key.name))
    });
    let Some(donor) = donor else {
        if !effects.is_empty() || !fx_materials.is_empty() {
            refusals.push(CommonDependencyRefusal::EffectDonor {
                effect: T6_FX_DONOR_EFFECT,
            });
        }
        return format!("t6 effects: no donor material ({T6_FX_DONOR_EFFECT} not loaded)");
    };
    let mut bound = 0usize;
    for (name, capture) in fx_materials {
        let Some(color) = capture.color else {
            continue;
        };
        let textures = asset_material::StandInTextures {
            color: Some((color.0, color.1, true)),
            normal: None,
            specular: None,
        };
        if materials
            .stand_in_material(donor.order(), &name, textures)
            .is_some()
        {
            bound += 1;
        }
    }
    let count = effects.len();
    let mut refused = Vec::new();
    for fx in &effects {
        let before = catalog.capture_gaps;
        catalog.capture_t6(fx, Iw4);
        if catalog.capture_gaps != before {
            refused.push(fx.name.as_str());
        }
    }
    format!("t6 effects bound: {count} effects, {bound} materials; not convertible: {refused:?}")
}
