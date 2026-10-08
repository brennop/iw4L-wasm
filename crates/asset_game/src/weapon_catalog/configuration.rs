use super::{WeaponRegistry, normalize_weapon_name};
use crate::{AssetNamespace, ConfigurationRefusal, FamilyKey, WeaponFamily, WeaponSelection};

pub(crate) trait WeaponConfigurationCompiler {
    fn compile(
        &self,
        family: &WeaponFamily,
        selection: &WeaponSelection,
    ) -> Result<u32, ConfigurationRefusal>;
}

pub(super) fn authored_name(key: &FamilyKey, attachments: &[String]) -> String {
    if attachments.is_empty() {
        return key.base.clone();
    }
    if key.namespace == AssetNamespace::T5 && attachments == ["dw"] {
        return format!("{}dw", key.base);
    }
    format!("{}_{}", key.base, attachments.join("_"))
}

struct AuthoredConfiguration<'a>(&'a WeaponRegistry);
struct Iw5Configuration<'a>(&'a WeaponRegistry);

impl WeaponConfigurationCompiler for AuthoredConfiguration<'_> {
    fn compile(
        &self,
        family: &WeaponFamily,
        selection: &WeaponSelection,
    ) -> Result<u32, ConfigurationRefusal> {
        let name = authored_name(&family.key, &selection.attachments);
        let id = self
            .0
            .by_namespaced
            .get(&(family.key.namespace, name.clone()))
            .copied()
            .ok_or_else(|| ConfigurationRefusal::MissingContent(format!("{name}_mp")))?;
        self.0.configuration_admission(id)?;
        Ok(id)
    }
}

impl WeaponConfigurationCompiler for Iw5Configuration<'_> {
    fn compile(
        &self,
        family: &WeaponFamily,
        selection: &WeaponSelection,
    ) -> Result<u32, ConfigurationRefusal> {
        if selection.attachments.is_empty() {
            return AuthoredConfiguration(self.0).compile(family, selection);
        }
        let base = family.base.ok_or_else(|| {
            ConfigurationRefusal::MissingContent(format!("{}_mp", family.key.base))
        })?;
        let slots = self
            .0
            .resolve_iw5_attachment_slots(base, &selection.attachments)?;
        self.0
            .iw5_primary_attachment_assets(base, slots)
            .ok_or_else(|| ConfigurationRefusal::MissingContent(self.0.name_of(base).into()))?;
        let id = self
            .0
            .configurations
            .get(selection)
            .copied()
            .ok_or_else(|| {
                ConfigurationRefusal::MissingContent(format!(
                    "{} {}",
                    family.key,
                    selection.attachments.join(" ")
                ))
            })?;
        self.0.configuration_admission(id)?;
        Ok(id)
    }
}

impl WeaponConfigurationCompiler for WeaponRegistry {
    fn compile(
        &self,
        family: &WeaponFamily,
        selection: &WeaponSelection,
    ) -> Result<u32, ConfigurationRefusal> {
        let compiler: &dyn WeaponConfigurationCompiler = match family.key.namespace {
            AssetNamespace::Iw5 => &Iw5Configuration(self),
            _ => &AuthoredConfiguration(self),
        };
        compiler.compile(family, selection)
    }
}

impl crate::weapon_families::FamilyContent for WeaponRegistry {
    fn lookup(&self, namespace: crate::AssetNamespace, name: &str) -> Option<u32> {
        self.by_namespaced
            .get(&(namespace, normalize_weapon_name(name)))
            .copied()
    }

    fn offhand_class(&self, id: u32) -> i32 {
        self.facts_of(id).map_or(0, |facts| facts.offhand_class)
    }

    fn admission(&self, id: u32) -> Result<(), crate::ConfigurationRefusal> {
        self.configuration_admission(id)
    }

    fn prepared_all(&self) -> Vec<(u32, crate::WeaponSelection)> {
        self.configurations
            .iter()
            .map(|(selection, &id)| (id, selection.clone()))
            .collect()
    }

    fn names_in(&self, namespace: crate::AssetNamespace) -> Vec<(u32, String)> {
        (1..=self.len() as u32)
            .filter(|&id| self.identity_namespace_of(id) == Some(namespace))
            .filter(|&id| self.iw5_configuration_of(id).is_none())
            .map(|id| (id, normalize_weapon_name(self.name_of(id))))
            .collect()
    }
}

pub(super) fn compile_completion_names(registry: &WeaponRegistry) -> Vec<String> {
    use crate::FamilySlot;
    let mut names: Vec<String> = registry
        .weapon_families()
        .offered()
        .filter(|family| matches!(family.slot, FamilySlot::Primary | FamilySlot::Secondary))
        .filter(|family| {
            family
                .base
                .is_some_and(|id| registry.gun_xmodel_of(id).is_some())
        })
        .map(|family| family.key.short())
        .collect();
    names.extend((1..registry.len() as u32).filter_map(|id| {
        if registry.describe_configuration(id).is_some()
            || registry.gun_xmodel_of(id).is_none()
            || registry.configuration_admission(id).is_err()
        {
            return None;
        }
        let facts = registry.hud_facts_of(id)?;
        if !facts.is_primary() || facts.offhand_class != 0 {
            return None;
        }
        let key = crate::FamilyKey::new(registry.namespace_of(id)?, registry.name_of(id));
        if registry.weapon_families().families().iter().any(|family| {
            family.key.namespace == key.namespace
                && (key.base == family.key.base
                    || key
                        .base
                        .strip_prefix(&family.key.base)
                        .is_some_and(|suffix| suffix.starts_with('_') || suffix == "dw"))
        }) {
            return None;
        }
        Some(key.short())
    }));
    names.sort();
    names.dedup();
    names
}
