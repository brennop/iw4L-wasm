mod effects;
mod models;
use effects::bind_t6_fx;
use models::bind_t6_content;

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
        refusals.extend(
            t6_dressed
                .refusals
                .iter()
                .cloned()
                .map(CommonDependencyRefusal::WeaponPreparation),
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
