use bevy::prelude::Resource;

use crate::asset_graph::DestructibleDeathRow;
use asset_anim::XAnimCatalog;
use asset_game::WeaponRegistry;
use asset_model::{BodyMeshCatalog, FpvMeshCatalog, WorldWeaponCatalog};

/// The one material population a prepared match owns, and the map-zone-local
/// index space that resolves into it.
///
/// Geometry keeps references — local material indices and, after the merge,
/// nothing else. The pool itself is never nested inside an optional product.
#[derive(Clone, Default)]
pub struct MatchMaterials {
    pub population: std::sync::Arc<asset_material::MaterialDefinitions>,

    pub common_profile_id: u64,

    pub products_id: u64,

    /// map-zone-local material index -> row in `population`
    pub map_ids: Vec<Option<usize>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PreparedGaps {
    pub lines: Vec<String>,
}

/// What the map itself declares about the match, captured once by the lane and
/// moved from there to its single owner in [`PreparedMap`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MapFacts {
    pub minimap_corners: Option<asset_world::MinimapCorners>,

    pub north_yaw: Option<f32>,

    pub airstrike_height: Option<f32>,

    pub compass: asset_world::MapCompassDeclaration,

    pub script_sound: asset_audio::MapScriptSoundFacts,

    pub team_settings: asset_game::MapTeamSettings,

    pub t5_teamset: Option<String>,

    pub objective_visuals: asset_game::ObjectiveVisuals,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PreparedMap {
    pub content: std::sync::Arc<asset_core::MapContentDefinition>,
    pub zone: String,

    pub namespace: Option<asset_core::AssetNamespace>,
    pub spawns: Vec<asset_world::SpawnPoint>,

    pub facts: MapFacts,
    pub gaps: PreparedGaps,
}

#[derive(Clone, Debug, Default, Resource)]
pub struct PreparedKillstreaks(pub Vec<String>);

#[derive(Clone, Debug, Default, Resource)]
pub struct PreparedWeapons(
    std::sync::Arc<WeaponRegistry>,
    Option<u32>,
    frame::WorldStamp,
);

#[derive(Clone, Copy)]
pub struct BoundWeapons<'a>(&'a PreparedWeapons);

impl<'a> BoundWeapons<'a> {
    pub fn row(&self, row: u32) -> Option<asset_game::BoundWeapon<'a>> {
        self.0.registry().bind_published_row(row).ok()
    }

    pub fn registry(&self) -> &'a std::sync::Arc<WeaponRegistry> {
        self.0.registry()
    }
}

impl PreparedWeapons {
    pub fn registry(&self) -> &std::sync::Arc<WeaponRegistry> {
        &self.0
    }

    pub fn for_match(registry: std::sync::Arc<WeaponRegistry>, key: frame::LocalLoadKey) -> Self {
        Self(
            registry,
            Some(key.match_key.match_epoch),
            frame::WorldStamp::from_install(key.local_load_request_id),
        )
    }

    pub fn snapshot_weapon(
        &self,
        epoch: Option<u32>,
        row: u32,
    ) -> Result<asset_game::BoundWeapon<'_>, asset_game::WeaponBindingRefusal> {
        self.for_snapshot(epoch)?
            .row(row)
            .ok_or(asset_game::WeaponBindingRefusal::UnknownWeapon)
    }

    pub fn for_event(
        &self,
        generation: frame::WorldStamp,
    ) -> Result<BoundWeapons<'_>, asset_game::WeaponBindingRefusal> {
        if generation.0.is_none() || generation != self.2 {
            return Err(asset_game::WeaponBindingRefusal::StaleSnapshot);
        }
        Ok(BoundWeapons(self))
    }

    pub fn for_snapshot(
        &self,
        epoch: Option<u32>,
    ) -> Result<BoundWeapons<'_>, asset_game::WeaponBindingRefusal> {
        if self.1.is_none() || epoch != self.1 {
            return Err(asset_game::WeaponBindingRefusal::StaleSnapshot);
        }
        Ok(BoundWeapons(self))
    }
}

#[derive(Clone, Debug, Default, Resource)]
pub struct MatchType10SoundHints(pub Vec<String>);

#[derive(Clone, Debug, Default, Resource)]
pub struct PreparedFpvMeshes(pub std::sync::Arc<FpvMeshCatalog>);

#[derive(Clone, Debug, Default, Resource)]
pub struct PreparedBodies(pub std::sync::Arc<BodyMeshCatalog>);

#[derive(Clone, Debug, Default, Resource)]
pub struct PreparedWorldWeapons(pub std::sync::Arc<WorldWeaponCatalog>);

#[derive(Clone, Debug, Default, Resource)]
pub struct PreparedProjectileMeshes(pub asset_model::ProjectileMeshCatalog);

#[derive(Clone, Debug, Default, Resource)]
pub struct PreparedXModelWalkCensus {
    pub walked_n: usize,

    pub unclassified_n: usize,

    pub projectile_names: Vec<String>,
}

impl PreparedXModelWalkCensus {
    pub fn projectile_walked_n(&self) -> usize {
        self.projectile_names.len()
    }

    pub fn projectile_names_csv(&self) -> String {
        self.projectile_names.join(",")
    }

    pub fn report_line(&self, source: &str) -> String {
        format!(
            "{source} XModels: {} unique names, {} unclassified (model_kind None); projectile_* walked: {} ({})",
            self.walked_n,
            self.unclassified_n,
            self.projectile_walked_n(),
            if self.projectile_names.is_empty() {
                "none".to_owned()
            } else {
                self.projectile_names_csv()
            }
        )
    }
}

#[derive(Clone, Debug, Default, Resource)]
pub struct PreparedXAnims(pub std::sync::Arc<XAnimCatalog>);

#[derive(Clone, Debug, Default, Resource)]
pub struct PreparedDestructibleDeath(pub Vec<DestructibleDeathRow>);

#[derive(Clone, Debug, Default, Resource)]
pub struct PreparedLocalizedStrings(pub asset_game::LocalizeCatalog);

#[derive(Clone, Debug, Default, Resource)]
pub struct SessionCompass {
    pub corners: Option<asset_world::MinimapCorners>,

    pub north_yaw: Option<f32>,

    pub declaration: asset_world::MapCompassDeclaration,
}

#[derive(Clone, Copy, Debug, Default, Resource, PartialEq, Eq)]
pub struct PreparedBodyClips(pub bool);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DependencyRole {
    Authority,
    Presentation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DependencyImportance {
    Required,
    Optional,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedDependency {
    pub key: String,
    pub source: String,
    pub role: DependencyRole,
    pub importance: DependencyImportance,
    pub available: bool,
}

#[derive(Clone, Debug, Resource)]
pub struct PreparedCapabilities {
    pub version: u32,
    pub products: frame::WorldProducts,
    pub dependencies: Vec<PreparedDependency>,
    pub script_fx: std::collections::BTreeSet<String>,
}

impl PreparedCapabilities {
    pub fn prepare(prepared: &crate::PreparedMatch) -> Self {
        let mut script_fx = std::collections::BTreeSet::new();
        let mut dependencies = Vec::new();
        for namespace in [
            asset_core::AssetNamespace::Iw4,
            asset_core::AssetNamespace::Iw5,
            asset_core::AssetNamespace::T5,
            asset_core::AssetNamespace::T6,
        ] {
            for name in prepared.fx.names() {
                if prepared.fx.get_in(namespace, name).is_none() {
                    continue;
                }
                let key = format!("{}:{name}", namespace.as_str()).to_ascii_lowercase();
                script_fx.insert(key.clone());
                script_fx.insert(name.to_ascii_lowercase());
                dependencies.push(PreparedDependency {
                    key,
                    source: format!("{} effect corpus", namespace.as_str()),
                    role: DependencyRole::Presentation,
                    importance: DependencyImportance::Optional,
                    available: true,
                });
            }
        }
        for (key, source) in prepared.scripts.fx_references() {
            let available = script_fx.contains(&key);
            script_fx.insert(key.clone());
            dependencies.push(PreparedDependency {
                key,
                source,
                role: DependencyRole::Presentation,
                importance: DependencyImportance::Optional,
                available,
            });
        }
        let has_crossbow = (1..=prepared.weapons.len() as u32)
            .filter_map(|id| prepared.weapons.key_of(id))
            .any(|key| {
                key.namespace == asset_core::AssetNamespace::T6 && key.name.contains("crossbow")
            });
        if has_crossbow {
            let effect = "weapon/crossbow/fx_trail_crossbow_blink_grn_os";
            for (key, available) in [
                (
                    format!("t5:{effect}"),
                    prepared
                        .fx
                        .get_in(asset_core::AssetNamespace::T5, effect)
                        .is_some(),
                ),
                (
                    "t5:wpn_crossbow_alert".into(),
                    prepared
                        .script_sound_aliases
                        .as_ref()
                        .is_some_and(|aliases| {
                            aliases
                                .get("t5:wpn_crossbow_alert")
                                .is_some_and(|facts| facts.is_some())
                        }),
                ),
            ] {
                dependencies.push(PreparedDependency {
                    key,
                    source: "t6 crossbow warning".into(),
                    role: DependencyRole::Presentation,
                    importance: DependencyImportance::Required,
                    available,
                });
            }
        }
        Self {
            version: 1,
            products: frame::WorldProducts::from_walk(prepared.materials.products_id),
            dependencies,
            script_fx,
        }
    }

    pub fn validate(&self, role: frame::RuntimeRole) -> Result<(), String> {
        for dependency in &self.dependencies {
            let required_role = match dependency.role {
                DependencyRole::Authority => role.runs_authority(),
                DependencyRole::Presentation => role.runs_client(),
            };
            if required_role
                && dependency.importance == DependencyImportance::Required
                && !dependency.available
            {
                return Err(format!(
                    "Required capability {} missing ({})",
                    dependency.key, dependency.source
                ));
            }
        }
        Ok(())
    }
}
