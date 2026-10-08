use std::sync::Arc;

use asset_game::WeaponRegistry;
use weapon_iw4::{HITLOC_COUNT, WeaponCombatFacts, WeaponHostRules};

pub(crate) struct PreparedSimWeapons {
    revision: u64,
    content: Arc<sim::SimWeaponContent>,
}

impl PreparedSimWeapons {
    pub(crate) fn content(&self) -> &Arc<sim::SimWeaponContent> {
        &self.content
    }

    pub(crate) fn owned_by(&self, weapons: &WeaponRegistry) -> bool {
        self.revision == weapons.revision()
    }
}

pub(crate) fn compile(
    weapons: &WeaponRegistry,
    world_models: &asset_model::WorldWeaponCatalog,
    global_location: Option<[f32; HITLOC_COUNT]>,
    aliases: Vec<(String, u32)>,
    pen_table: weapon_iw4::PenetrationDepthTable,
    pen_table_loaded: bool,
) -> Result<PreparedSimWeapons, sim::SimWeaponContentRefusal> {
    let rules = WeaponHostRules::default();
    let mut refused = Vec::new();
    let groups = weapons.configuration_transition_groups();
    let rows = weapons.published_weapons().map(|weapon| {
        let id = weapon.wire_id();
        let combat = compile_combat(weapons, weapon, rules, global_location, &mut refused);
        let setup = weapons.describe_configuration(id).and_then(|selection| {
            let family = selection.family.as_ref()?;
            Some(sim::WeaponSetup {
                realm: family.namespace,
                base: family.base.clone(),
                attachments: selection.attachments.clone(),
                stand_in: None,
            })
        });
        let shield_model = weapon
            .world_facts()
            .filter(|facts| facts.is_shield())
            .and_then(|_| weapons.world_model_entry(id, world_models))
            .and_then(|entry| entry.skel.retained_capability())
            .map(Arc::new);
        let script_sounds = weapons
            .sounds_of(id)
            .map(|sounds| sim::WeaponScriptSounds {
                fire: sounds.fire.clone(),
                fire_player: sounds.fire_player.clone(),
                pickup: sounds.pickup.clone(),
                pickup_player: sounds.pickup_player.clone(),
                proj_explosion: sounds.proj_explosion.clone(),
            })
            .unwrap_or(sim::WeaponScriptSounds::default());
        sim::SimWeaponRow {
            wire_id: id,
            scales: weapon.movement_scales(),
            combat,
            runnable: id != 0 && weapons.configuration_admission(id).is_ok(),
            transition_group: groups[id as usize],
            penetration: weapons
                .penetration_facts_of(id)
                .unwrap_or(weapon_iw4::BulletPenFacts::default()),
            script_name: weapons.script_name_of(id),
            setup,
            world_model: (
                weapons.world_model_of(id).unwrap_or("").to_owned(),
                weapons.hide_tags_of(id).to_vec(),
            ),
            shield_model,
            projectile_model: weapons.projectile_model_of(id).unwrap_or("").to_owned(),
            melee_only: weapons.melee_only_of(id),
            script_sounds,
            equipment: weapons
                .equipment_facts_of(id)
                .unwrap_or(sim::EquipmentRuntimeFacts::default()),
        }
    });
    let content = sim::SimWeaponContent::compile(rows, aliases, pen_table, pen_table_loaded)?;
    if !refused.is_empty() {
        diag::info!(
            Sim,
            "combat facts refused for {} weapons: {}",
            refused.len(),
            refused.join(" ")
        );
    }
    Ok(PreparedSimWeapons {
        revision: weapons.revision(),
        content,
    })
}

fn compile_combat(
    weapons: &WeaponRegistry,
    weapon: asset_game::BoundWeapon<'_>,
    rules: WeaponHostRules,
    global_location: Option<[f32; HITLOC_COUNT]>,
    refused: &mut Vec<String>,
) -> WeaponCombatFacts {
    if weapon.wire_id() == 0 {
        return WeaponCombatFacts::none();
    }
    match weapon.combat_facts(rules, global_location) {
        Ok(facts) => facts,
        Err(reason) => {
            refused.push(format!("{}({reason:?})", weapons.name_of(weapon.wire_id())));
            WeaponCombatFacts::none()
        }
    }
}

pub struct ClassWeaponAdmission<'a> {
    registry: &'a WeaponRegistry,
    combat: Vec<WeaponCombatFacts>,
    equipment: Vec<sim::EquipmentRuntimeFacts>,
}

impl<'a> ClassWeaponAdmission<'a> {
    pub fn prepare(registry: &'a WeaponRegistry) -> Self {
        let mut combat = Vec::new();
        let mut equipment = Vec::new();
        let mut refused = Vec::new();
        for weapon in registry.published_weapons() {
            combat.push(compile_combat(
                registry,
                weapon,
                WeaponHostRules::default(),
                None,
                &mut refused,
            ));
            equipment.push(
                registry
                    .equipment_facts_of(weapon.wire_id())
                    .unwrap_or(sim::EquipmentRuntimeFacts::default()),
            );
        }
        if !refused.is_empty() {
            diag::info!(
                Sim,
                "combat facts refused for {} weapons: {}",
                refused.len(),
                refused.join(" ")
            );
        }
        Self {
            registry,
            combat,
            equipment,
        }
    }

    pub fn allows(&self, row: &crate::ClassRow) -> bool {
        !crate::project_class(0, row, self.registry, &self.combat, &self.equipment)
            .def
            .locked
    }
}
