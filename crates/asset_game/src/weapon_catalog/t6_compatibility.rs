use super::*;

impl WeaponBuild {
    pub fn dress_t6_stand_ins(
        &mut self,
        own_view: impl Fn(&str) -> bool,
        own_world: impl Fn(&str) -> bool,
        own_sound: impl Fn(&str) -> bool,
        own_anim: impl Fn(&str) -> bool,
        hands: Option<&str>,
        melee: Option<&T6Melee>,
    ) -> T6StandInCensus {
        let mut census = T6StandInCensus::default();
        let left_halves: HashSet<String> = self
            .registry
            .rows
            .iter()
            .filter(|row| row.namespace == crate::AssetNamespace::T6)
            .filter(|row| xanims_idle(&row.sz_xanims_left).is_some())
            .map(|row| row.name.clone())
            .collect();
        for index in 1..self.registry.rows.len() {
            let own = &self.registry.rows[index];
            if own.namespace != crate::AssetNamespace::T6 {
                continue;
            }
            let donor = crate::weapon_t6::stand_in_for(&own.name).and_then(|name| {
                self.registry
                    .by_namespaced
                    .get(&(crate::AssetNamespace::Iw4, normalize_weapon_name(name)))
                    .copied()
            });
            let Some(donor) = donor else {
                census.missing.push(own.name.clone());
                census
                    .refusals
                    .push(WeaponPreparationRefusal::MissingWeaponDonor {
                        weapon: own.name.clone(),
                        donor: crate::weapon_t6::stand_in_for(&own.name).map(str::to_owned),
                    });
                let row = &mut self.registry.rows[index];
                row.preparation =
                    WeaponPreparationRecipe::for_capture(crate::AssetNamespace::T6, &row.name);
                row.fpv_hands = [None, None];
                row.fpv_mount_plan = None;
                row.fpv_assemblies = [None, None];
                continue;
            };
            let donor = donor as usize;
            let donor_key = asset_core::AssetKey {
                namespace: crate::AssetNamespace::Iw4,
                kind: asset_core::AssetKind::Weapon,
                name: self.registry.rows[donor].name.clone(),
            };
            let mut dressed = self.registry.rows[donor].clone();
            let own = std::mem::take(&mut self.registry.rows[index]);
            let stand_in_facts = std::mem::replace(&mut dressed.facts, own.facts);
            let facts = &mut dressed.facts;
            facts.player_anim_type = stand_in_facts.player_anim_type;
            facts.impact_type = stand_in_facts.impact_type;
            facts.movement = stand_in_facts.movement;
            facts.ducked_ofs = stand_in_facts.ducked_ofs;
            facts.prone_ofs = stand_in_facts.prone_ofs;
            facts.knife_model = stand_in_facts.knife_model;
            facts.sprint_raise_time_ms = stand_in_facts.sprint_raise_time_ms;
            facts.sprint_loop_time_ms = stand_in_facts.sprint_loop_time_ms;
            facts.sprint_drop_time_ms = stand_in_facts.sprint_drop_time_ms;
            facts.dual_wield_view_model_offset = stand_in_facts.dual_wield_view_model_offset;
            facts.i_reticle_side_size = stand_in_facts.i_reticle_side_size;
            facts.i_reticle_min_ofs = stand_in_facts.i_reticle_min_ofs;
            facts.ads_aim_pitch = stand_in_facts.ads_aim_pitch;
            facts.ads_crosshair_in_frac = stand_in_facts.ads_crosshair_in_frac;
            facts.ads_crosshair_out_frac = stand_in_facts.ads_crosshair_out_frac;
            if facts.hip_reticle_side_pos == 0.0 {
                facts.hip_reticle_side_pos = stand_in_facts.hip_reticle_side_pos;
            }
            if facts.offhand_class != 0 && stand_in_facts.offhand_class != 0 {
                facts.offhand_class = stand_in_facts.offhand_class;
            }
            if crate::weapon_t6::is_tactical_equipment(&own.name) {
                facts.offhand_class = OFFHAND_CLASS_SMOKE;
            }
            if let Some(effect) = own
                .combat_fx
                .explosion_hint
                .as_deref()
                .filter(|name| crate::T6_EFFECTS.contains(name))
            {
                dressed.combat_fx.explosion_hint = Some(effect.to_owned());
            }
            if crate::weapon_t6::sheds_stand_in_trail(&own.name) {
                dressed.proj_trail = None;
                dressed.proj_beacon = None;
            }
            if crate::weapon_t6::stays_planted(&own.name) {
                facts.timed_detonation = false;
                if facts.stickiness == 0 {
                    facts.stickiness = 3;
                }
            }
            facts.fire_melees = facts.weap_type == weapon_iw4::WEAPTYPE_SHIELD;
            if facts.penetrate_multiplier == 0.0 {
                facts.penetrate_multiplier = if stand_in_facts.penetrate_multiplier > 0.0 {
                    stand_in_facts.penetrate_multiplier
                } else {
                    1.0
                };
            }
            if facts.parallel_bounce.is_none() || facts.perpendicular_bounce.is_none() {
                facts.parallel_bounce = stand_in_facts.parallel_bounce;
                facts.perpendicular_bounce = stand_in_facts.perpendicular_bounce;
            }
            let mut own_gun = false;
            if let Some(gun) = own.gun_xmodel.filter(|name| own_view(name)) {
                dressed.gun_xmodel = Some(gun);
                census.own_view += 1;
                own_gun = true;
                dressed.attachment_view_models = own
                    .attachment_view_models
                    .iter()
                    .filter(|name| own_view(name))
                    .cloned()
                    .collect();
            }
            let own_anims = own_gun
                && own.sz_xanims[weap_anim::IDLE]
                    .as_deref()
                    .is_some_and(&own_anim);
            let hands = hands.filter(|_| own_anims);
            if let Some(hands) = hands {
                dressed.sz_xanims = own.sz_xanims.map(|name| name.filter(|name| own_anim(name)));
                dressed.sz_xanims_right = [const { None }; WEAPON_ANIM_SLOTS];
                dressed.sz_xanims_left = [const { None }; WEAPON_ANIM_SLOTS];
                dressed.hand_xmodel = Some(hands.to_owned());
                census.own_anims += 1;
                dressed.knife_xmodel = None;
                if dressed.sz_xanims[weap_anim::MELEE]
                    .as_deref()
                    .is_none_or(|clip| clip.contains("tactical_melee"))
                    && let Some(melee) = melee
                    && own_view(&melee.knife)
                    && own_anim(&melee.melee)
                {
                    let charge = melee.charge.as_ref().filter(|name| own_anim(name));
                    dressed.sz_xanims[weap_anim::MELEE] = Some(melee.melee.clone());
                    dressed.sz_xanims[weap_anim::MELEE_CHARGE] =
                        Some(charge.unwrap_or(&melee.melee).clone());
                    dressed.knife_xmodel = Some(melee.knife.clone());
                    census.borrowed_melee += 1;
                }
                if let Some(left) = own
                    .name
                    .strip_suffix("_dw")
                    .map(|base| format!("{base}_lh"))
                    .filter(|left| left_halves.contains(left))
                {
                    dressed.dual_wield_weapon = Some(left);
                    dressed.facts.dual_wield = true;
                    dressed.facts.no_dual_wield = false;
                    dressed.facts.dual_wield_view_model_offset = 0.0;
                    census.dual_wield += 1;
                }
            }
            if own_gun
                && own.sz_xanims_left[weap_anim::IDLE]
                    .as_deref()
                    .is_some_and(&own_anim)
            {
                dressed.sz_xanims_left = own
                    .sz_xanims_left
                    .clone()
                    .map(|name| name.filter(|name| own_anim(name)));
            }
            if let Some(world) = own.world_model.filter(|name| own_world(name)) {
                dressed.world_model = Some(world);
                census.own_world += 1;
                dressed.attachment_world_models = own
                    .attachment_world_models
                    .iter()
                    .filter(|name| own_world(name))
                    .cloned()
                    .collect();
            }
            if let Some(projectile) = own.projectile_model.filter(|name| own_world(name)) {
                dressed.projectile_model = Some(projectile);
                census.own_projectile += 1;
            }
            let mut sounds = own.sounds;
            keep_t6_sounds(&mut sounds, &own_sound);
            if sounds.fire.is_some() || sounds.fire_player.is_some() {
                census.own_sounds += 1;
            }
            merge_sound_aliases(&mut sounds, &dressed.sounds);
            if hands.is_some() {
                sounds.notetrack_sound_map.clear();
                sounds.notetrack_rumble_map.clear();
                sounds.notetrack_convention = NotetrackConvention::InlinePrefix;
            } else {
                sounds.notetrack_sound_map =
                    std::mem::take(&mut dressed.sounds.notetrack_sound_map);
                sounds.notetrack_rumble_map =
                    std::mem::take(&mut dressed.sounds.notetrack_rumble_map);
                sounds.notetrack_convention = dressed.sounds.notetrack_convention;
            }
            dressed.sounds = sounds;
            if own_gun {
                dressed.hide_tags = own
                    .t6_attachments
                    .iter()
                    .find(|attachment| attachment.kind == 0 && attachment.mask == 0)
                    .map_or_else(Vec::new, |bare| bare.hide_tags.clone());
            }
            dressed.t6_clip_models = [
                own.t6_clip_models[0].clone().filter(|name| own_view(name)),
                own.t6_clip_models[1].clone().filter(|name| own_world(name)),
            ];
            dressed.t6_attachments = own
                .t6_attachments
                .iter()
                .cloned()
                .map(|mut attachment| {
                    attachment.models[0].retain(|name| own_gun && own_view(name));
                    attachment.models[1].retain(|name| own_world(name));
                    attachment.xanims = attachment
                        .xanims
                        .map(|name| name.filter(|name| hands.is_some() && own_anim(name)));
                    for sound in [
                        &mut attachment.fire_sound,
                        &mut attachment.fire_sound_player,
                    ] {
                        *sound = sound.take().filter(|name| own_sound(name));
                    }
                    attachment
                })
                .collect();
            dressed.t6_attachment_stats = own.t6_attachment_stats.clone();
            if dressed.facts.offhand_class != 0
                && let Some(icon) = own.hud_icon.clone()
            {
                dressed.hud_icon_image = Some(icon.clone());
                dressed.hud_icon = Some(icon);
                dressed.hud_icon_from_slot = false;
                dressed.hud_icon_ratio = 0;
                dressed.pickup_icon_authored = false;
                dressed.own_hud_icon = true;
            }
            dressed.overlay_material = own.overlay_material;
            dressed.overlay_image = own.overlay_image;
            dressed.overlay_material_from_slot = false;
            dressed.name = own.name;
            dressed.namespace = crate::AssetNamespace::T6;
            dressed.alternate_weapon = own.alternate_weapon;
            dressed.alternate_index = 0;
            dressed.display_name_key = own.display_name_key;
            dressed.preparation = WeaponPreparationRecipe::t6(
                &dressed, donor_key, &own_view, &own_world, &own_sound, &own_anim, hands,
            );
            self.registry.rows[index] = dressed;
            if let Some(&slots) = self.combat_slots.get(donor)
                && let Some(own_slots) = self.combat_slots.get_mut(index)
            {
                *own_slots = slots;
            }
            census.dressed += 1;
        }
        self.registry.rebuild_name_maps();
        self.registry.revision = mint_weapon_revision();
        census
    }
}
