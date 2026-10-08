use bevy::platform::collections::HashMap;
use bevy::prelude::Resource;
use render_frame::TextureBindIdentity;
use render_material::MaterialGenerationId;
use std::sync::Arc;

use crate::drawsurf::texture_table::FixedTextureGroup;

pub(super) type ColourBindingLanes = [HashMap<BoundTextureKey, BoundTextures>; 4];

/// A draw's textures: slot words into its texture table and, in fixed-slot form, the group 1
/// built from them.
#[derive(Clone)]
pub(super) struct BoundTextures {
    pub(super) slots: Arc<[u32]>,
    pub(super) group: Option<FixedTextureGroup>,
}

#[derive(Resource, Default)]
pub(super) struct ExactColourBindingCache {
    pub(super) generation: MaterialGenerationId,
    views_revision: u64,
    pub(super) textures: ColourBindingLanes,
}

#[derive(Resource, Default)]
pub(super) struct ExactShadowBindingCache {
    pub(super) generation: MaterialGenerationId,
    views_revision: u64,
    pub(super) textures: HashMap<BoundTextureKey, BoundTextures>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct BoundTextureKey {
    pub(super) identity: TextureBindIdentity,
    pub(super) spot_shadow_select: Option<u8>,
}

impl ExactColourBindingCache {
    pub(super) fn open_epoch(&mut self, generation: MaterialGenerationId, views_revision: u64) {
        if self.generation == generation && self.views_revision == views_revision {
            return;
        }
        self.generation = generation;
        self.views_revision = views_revision;
        for slots in &mut self.textures {
            slots.clear();
        }
    }

    pub(super) fn interned_n(&self) -> usize {
        self.textures.iter().map(HashMap::len).sum()
    }
}

impl ExactShadowBindingCache {
    pub(super) fn open_epoch(&mut self, generation: MaterialGenerationId, views_revision: u64) {
        if self.generation == generation && self.views_revision == views_revision {
            return;
        }
        self.generation = generation;
        self.views_revision = views_revision;
        self.textures.clear();
    }
}
