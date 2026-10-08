use super::*;

impl WeaponPreparationRecipe {
    pub(in crate::weapon_catalog) fn t6(
        row: &WeaponRow,
        donor: AssetKey,
        own_view: &impl Fn(&str) -> bool,
        own_world: &impl Fn(&str) -> bool,
        own_sound: &impl Fn(&str) -> bool,
        own_anim: &impl Fn(&str) -> bool,
        hands: Option<&str>,
    ) -> Self {
        let mut recipe = Self::native(AssetNamespace::Iw4);
        recipe.donor_weapon = Some(donor.clone());
        recipe.closed_references = true;
        recipe.attachment_mount_on_root = true;
        recipe.hide_mode = crate::FpvHideMode::Bones;
        let mut add = |component, name: &str, converted: bool| {
            if name.is_empty() {
                return;
            }
            if recipe
                .references
                .iter()
                .any(|r| r.component == component && r.name == name)
            {
                return;
            }
            recipe.references.push(PreparedComponentReference {
                component,
                source_name: if converted && component == WeaponComponent::Animation {
                    name.strip_prefix(crate::T6_XANIM_PREFIX)
                        .unwrap_or(name)
                        .to_owned()
                } else {
                    name.to_owned()
                },
                name: name.to_owned(),
                storage_namespace: AssetNamespace::Iw4,
                source_namespace: if converted {
                    AssetNamespace::T6
                } else {
                    AssetNamespace::Iw4
                },
                target: if component == WeaponComponent::Sound {
                    PreparedComponentTarget::SoundPublication
                } else {
                    PreparedComponentTarget::Pending
                },
                policy: if converted {
                    ComponentPreparationPolicy::T6Conversion
                } else {
                    ComponentPreparationPolicy::T6WeaponDonor {
                        weapon: donor.clone(),
                    }
                },
            });
        };
        for name in row
            .gun_xmodel
            .iter()
            .chain(&row.secondary_gun_xmodel)
            .chain(&row.rocket_model)
            .chain(&row.knife_xmodel)
            .chain(&row.attachment_view_models)
            .chain(row.camo_models.view.iter().map(|(_, name)| name))
        {
            add(WeaponComponent::ViewModel, name, own_view(name));
        }
        for name in row.hand_xmodel.iter() {
            add(WeaponComponent::Hands, name, hands == Some(name.as_str()));
        }
        for name in row
            .world_model
            .iter()
            .chain(&row.attachment_world_models)
            .chain(row.camo_models.world.iter().map(|(_, name)| name))
        {
            add(WeaponComponent::WorldModel, name, own_world(name));
        }
        for name in row.projectile_model.iter() {
            add(WeaponComponent::Projectile, name, own_world(name));
        }
        for name in row
            .sz_xanims
            .iter()
            .chain(&row.sz_xanims_right)
            .chain(&row.sz_xanims_left)
            .flatten()
        {
            add(WeaponComponent::Animation, name, own_anim(name));
        }
        for attachment in &row.t6_attachments {
            for name in &attachment.models[0] {
                add(WeaponComponent::ViewModel, name, true);
            }
            for name in &attachment.models[1] {
                add(WeaponComponent::WorldModel, name, true);
            }
            for name in attachment.xanims.iter().flatten() {
                add(WeaponComponent::Animation, name, true);
            }
            for name in attachment
                .fire_sound
                .iter()
                .chain(&attachment.fire_sound_player)
            {
                add(WeaponComponent::Sound, name, true);
            }
            if let Some((model, ads)) = &attachment.view_ads_model {
                add(WeaponComponent::ViewModel, model, true);
                add(WeaponComponent::ViewModel, ads, true);
            }
        }
        if let Some(name) = &row.t6_clip_models[0] {
            add(WeaponComponent::ViewModel, name, own_view(name));
        }
        if let Some(name) = &row.t6_clip_models[1] {
            add(WeaponComponent::WorldModel, name, own_world(name));
        }
        for name in row.sounds.reachable_aliases() {
            add(WeaponComponent::Sound, name, own_sound(name));
        }
        for name in row
            .overlay_material
            .iter()
            .chain(&row.hud_icon)
            .chain(&row.pickup_icon)
            .chain(&row.kill_icon)
            .chain(&row.reticle.center_material)
            .chain(&row.reticle.side_material)
        {
            let captured = row.overlay_material.as_deref() == Some(name.as_str())
                || (row.own_hud_icon && row.hud_icon.as_deref() == Some(name.as_str()));
            add(WeaponComponent::Material, name, captured);
        }
        for name in row
            .proj_trail
            .iter()
            .chain(&row.proj_beacon)
            .chain(&row.proj_ignition)
            .chain(&row.combat_fx.view_flash_hint)
            .chain(&row.combat_fx.world_flash_hint)
            .chain(&row.combat_fx.view_shell_eject_hint)
            .chain(&row.combat_fx.world_shell_eject_hint)
            .chain(&row.combat_fx.view_last_shot_eject_hint)
            .chain(&row.combat_fx.world_last_shot_eject_hint)
            .chain(&row.combat_fx.explosion_hint)
        {
            add(
                WeaponComponent::Effect,
                name,
                crate::T6_EFFECTS.contains(&name.as_str()),
            );
        }
        recipe.routes[WeaponComponent::OverlayImage as usize] = Some(AssetNamespace::T6);
        if row.own_hud_icon {
            recipe.routes[WeaponComponent::HudIconImage as usize] = Some(AssetNamespace::T6);
        }
        for (component, image) in [
            (WeaponComponent::OverlayImage, row.overlay_image.as_ref()),
            (
                WeaponComponent::HudIconImage,
                row.hud_icon_image.as_ref().filter(|_| row.own_hud_icon),
            ),
        ] {
            if let Some(name) = image {
                recipe.references.push(PreparedComponentReference {
                    component,
                    source_namespace: AssetNamespace::T6,
                    source_name: name.clone(),
                    name: name.clone(),
                    storage_namespace: AssetNamespace::T6,
                    policy: ComponentPreparationPolicy::Native,
                    target: PreparedComponentTarget::UiImages,
                });
            }
        }
        recipe.set_source(AssetNamespace::T6, &row.name);
        recipe
    }
}
