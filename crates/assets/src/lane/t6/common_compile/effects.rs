use crate::lane::CommonDependencyRefusal;
use asset_material::MaterialCatalog;

const T6_FX_DONOR_EFFECT: &str = "misc/glow_stick_glow_green";

pub(super) fn bind_t6_fx(
    effects: Vec<asset_game::T6FxCapture>,
    fx_materials: std::collections::BTreeMap<String, super::super::T6MaterialCapture>,
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
            .t6_donor_surface(
                donor.order(),
                asset_core::MaterialKey {
                    namespace: asset_core::AssetNamespace::T6,
                    name: name.clone(),
                },
                &name,
                textures,
            )
            .is_ok()
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
