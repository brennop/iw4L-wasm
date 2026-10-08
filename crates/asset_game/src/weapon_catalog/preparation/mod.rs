use super::*;
mod binding;
mod t6;
use asset_core::{AssetKey, AssetKind, AssetNamespace};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum WeaponComponent {
    ViewModel,
    WorldModel,
    Hands,
    Animation,
    Material,
    Effect,
    Projectile,
    Sound,
    HudIconImage,
    OverlayImage,
    Tracer,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComponentPreparationPolicy {
    Native,
    T6Conversion,
    T6WeaponDonor { weapon: AssetKey },
    Unsupported(ComponentPreparationRefusal),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComponentPreparationRefusal {
    UndeclaredReference,
    CatalogMiss,
    UnrequestedCapability,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreparedComponentTarget {
    Pending,
    Fpv(AssetEdge<FpvMeshSpace>),
    World(AssetEdge<WorldWeaponSpace>),
    Animation(AssetEdge<XAnimSpace>),
    Material(AssetEdge<MaterialSpace>),
    Effect(AssetEdge<FxSpace>),
    Projectile(AssetEdge<ProjectileModelSpace>),
    Tracer(AssetEdge<TracerSpace>),
    SoundPublication,
    UiImages,
    Unsupported(ComponentPreparationRefusal),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WeaponPreparationRefusal {
    MissingWeaponDonor {
        weapon: String,
        donor: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedComponentReference {
    component: WeaponComponent,
    source_namespace: AssetNamespace,
    source_name: String,
    name: String,
    storage_namespace: AssetNamespace,
    policy: ComponentPreparationPolicy,
    target: PreparedComponentTarget,
}

impl PreparedComponentReference {
    pub fn target(&self) -> PreparedComponentTarget {
        self.target
    }
    pub fn component(&self) -> WeaponComponent {
        self.component
    }
    pub fn source_name(&self) -> &str {
        &self.source_name
    }
    pub fn source_namespace(&self) -> AssetNamespace {
        self.source_namespace
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn storage_namespace(&self) -> AssetNamespace {
        self.storage_namespace
    }
    pub fn policy(&self) -> &ComponentPreparationPolicy {
        &self.policy
    }
}

#[derive(Clone, Debug)]
pub struct WeaponPreparationRecipe {
    host_namespace: AssetNamespace,
    attachment_mount_on_root: bool,
    hide_mode: crate::FpvHideMode,
    source: AssetKey,
    routes: [Option<AssetNamespace>; 11],
    references: Vec<PreparedComponentReference>,
    refusal: Option<WeaponPreparationRefusal>,
    donor_weapon: Option<AssetKey>,
    closed_references: bool,
}

impl WeaponPreparationRecipe {
    pub fn host_namespace(&self) -> AssetNamespace {
        self.host_namespace
    }
    pub fn attachment_mount_on_root(&self) -> bool {
        self.attachment_mount_on_root
    }
    pub fn hide_mode(&self) -> crate::FpvHideMode {
        self.hide_mode
    }
    pub fn ads_overlay_convention(&self) -> crate::AdsOverlayConvention {
        crate::AdsOverlayConvention::from_namespace(self.host_namespace)
    }
    pub fn source(&self) -> &AssetKey {
        &self.source
    }
    pub(super) fn set_source(&mut self, namespace: AssetNamespace, name: &str) {
        self.source.namespace = namespace;
        self.source.name = name.to_owned();
    }
    pub fn references(&self) -> &[PreparedComponentReference] {
        &self.references
    }
    pub fn refusal(&self) -> Option<&WeaponPreparationRefusal> {
        self.refusal.as_ref()
    }
    pub fn namespace(&self, component: WeaponComponent) -> Option<AssetNamespace> {
        self.routes[component as usize]
    }
    pub(super) fn native(namespace: AssetNamespace) -> Self {
        Self {
            host_namespace: namespace,
            attachment_mount_on_root: false,
            hide_mode: crate::FpvHideMode::Surfaces,
            source: AssetKey {
                namespace,
                kind: AssetKind::Weapon,
                name: String::new(),
            },
            routes: [Some(namespace); 11],
            references: Vec::new(),
            refusal: None,
            donor_weapon: None,
            closed_references: false,
        }
    }
    pub(super) fn for_capture(namespace: AssetNamespace, weapon: &str) -> Self {
        if namespace != AssetNamespace::T6 {
            return Self::native(namespace);
        }
        Self {
            host_namespace: AssetNamespace::Iw4,
            attachment_mount_on_root: true,
            hide_mode: crate::FpvHideMode::Bones,
            source: AssetKey {
                namespace,
                kind: AssetKind::Weapon,
                name: weapon.to_owned(),
            },
            donor_weapon: None,
            closed_references: true,
            routes: [None; 11],
            references: Vec::new(),
            refusal: Some(WeaponPreparationRefusal::MissingWeaponDonor {
                weapon: weapon.to_owned(),
                donor: crate::weapon_t6::stand_in_for(weapon).map(str::to_owned),
            }),
        }
    }
}
