use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

use asset_core::AssetNamespace;

static IMAGES: LazyLock<RwLock<HashMap<(AssetNamespace, String), String>>> =
    LazyLock::new(Default::default);
static PREVIEW_FALLBACKS: LazyLock<RwLock<HashMap<(AssetNamespace, String), String>>> =
    LazyLock::new(Default::default);

pub fn retain_ui_preview_fallback(namespace: AssetNamespace, material: &str, image: &str) {
    PREVIEW_FALLBACKS
        .write()
        .unwrap_or_else(|poison| poison.into_inner())
        .insert((namespace, material.to_ascii_lowercase()), image.to_owned());
}

pub fn ui_preview_fallback(namespace: AssetNamespace, material: &str) -> Option<String> {
    PREVIEW_FALLBACKS
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
        .get(&(
            namespace,
            asset_core::AssetRef::bare_name(material).to_ascii_lowercase(),
        ))
        .cloned()
}

pub fn retain_ui_material_images(materials: &crate::MaterialCatalog) {
    let mut images = IMAGES.write().unwrap_or_else(|poison| poison.into_inner());
    for material in &materials.materials {
        if let Some(image) = materials.hud_image_name(material) {
            images.insert(
                (
                    material.namespace,
                    material.name.as_str().to_ascii_lowercase(),
                ),
                image.to_owned(),
            );
        }
    }
}

pub fn ui_material_image(namespace: AssetNamespace, material: &str) -> Option<String> {
    IMAGES
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
        .get(&(
            namespace,
            asset_core::AssetRef::bare_name(material).to_ascii_lowercase(),
        ))
        .cloned()
}
