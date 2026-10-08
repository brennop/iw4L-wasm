use bevy::math::{Mat4, Vec3, Vec4};
use render_material::{
    MaterialGenerationId, RuntimeArgumentBinding, RuntimeCodeSources, RuntimeMaterialCatalog,
};
mod fog;
mod local_light;
pub use local_light::{MaterialLightOverrides, MaterialLocalLightInputs};

#[derive(Clone, Debug)]
pub struct PreparedMaterialBindings {
    generation: MaterialGenerationId,
    requested: Vec<u16>,
}

#[derive(Clone, Copy, Debug)]
pub struct MaterialFrameBindingInputs {
    pub eye: Vec3,
    pub clip_from_view: Mat4,
    pub world_from_view: Mat4,
    pub target_size: [i32; 2],
    pub time: f32,
    pub exposure: Option<f32>,
    pub exposure_stops: Option<f32>,
    pub model_lighting_decode_scale: f32,
    pub reflection_probe_alpha_weight: f32,
    pub sky_intensity: Option<[f32; 4]>,
    pub tree_scatter: Option<[f32; 2]>,
    pub fog: MaterialFogInputs,
    pub fog_enabled: bool,
    pub sun: Option<MaterialSunInputs>,
}

#[derive(Clone, Copy, Debug)]
pub struct MaterialSunInputs {
    pub direction: [f32; 3],
    pub color: [f32; 3],
    pub diffuse_color: Option<[f32; 4]>,
    pub specular_color: Option<[f32; 4]>,
}

#[derive(Clone, Copy, Debug)]
pub struct MaterialFogInputs {
    pub color_rgb: [f32; 3],
    pub max_opacity: f32,
    pub halfway_dist: f32,
    pub start_dist: f32,
    pub volumetric: Option<MaterialFogVolumeInputs>,
    pub sun: Option<MaterialFogSunInputs>,
}

#[derive(Clone, Copy, Debug)]
pub struct MaterialFogVolumeInputs {
    pub halfway_height: f32,
    pub base_height: f32,
    pub color_scale: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct MaterialFogSunInputs {
    pub color_rgb: [f32; 3],
    pub sun_dir: [f32; 3],
    pub begin_angle_deg: f32,
    pub end_angle_deg: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StaleMaterialBindings {
    pub prepared: MaterialGenerationId,
    pub current: MaterialGenerationId,
}

pub fn compile_material_bindings(catalog: &RuntimeMaterialCatalog) -> PreparedMaterialBindings {
    let mut requested: Vec<_> = catalog
        .technique_sets
        .iter()
        .flat_map(|set| set.techniques())
        .flat_map(|technique| &technique.passes)
        .flat_map(|pass| &pass.arguments)
        .filter_map(|argument| match argument {
            RuntimeArgumentBinding::CodeConstant { index, .. } => Some(*index),
            _ => None,
        })
        .collect();
    requested.sort_unstable();
    requested.dedup();
    PreparedMaterialBindings {
        generation: catalog.generation_id,
        requested,
    }
}

impl PreparedMaterialBindings {
    pub fn generation_id(&self) -> MaterialGenerationId {
        self.generation
    }

    pub fn bind_frame(
        &self,
        generation: MaterialGenerationId,
        sources: &mut RuntimeCodeSources,
        inputs: &MaterialFrameBindingInputs,
    ) -> Result<(), StaleMaterialBindings> {
        if generation != self.generation {
            return Err(StaleMaterialBindings {
                prepared: self.generation,
                current: generation,
            });
        }
        let mut writer = BindingWriter {
            requested: &self.requested,
            sources,
        };
        let sources = &mut writer;
        produce_leftover_iw5_code_consts(sources, inputs.eye);
        produce_leftover_t5_code_consts(
            sources,
            inputs.eye,
            inputs.clip_from_view,
            inputs.world_from_view,
            inputs.target_size[0],
            inputs.target_size[1],
        );
        fog::produce(sources, &inputs.fog, inputs.eye.z, inputs.fog_enabled);
        if let Some(light) = &inputs.sun {
            produce_leftover_t5_sun_constants(sources, light);
        }
        let hdr_exposure = inputs.exposure.unwrap_or(T5_HDRCONTROL_HOST_EXPOSURE);
        produce_leftover_t5_hdrcontrol(sources, hdr_exposure);
        let t6_exposure = inputs.exposure_stops.unwrap_or(0.0);
        let reciprocal = t6_exposure.exp2();
        sources.set_constant_rows(
            crate::t6_techset::CODE_T6_HDR_CONTROL_0,
            &[float4_bits([
                reciprocal.recip(),
                0.0,
                reciprocal,
                reciprocal,
            ])],
        );
        sources.set_constant_rows(
            crate::t6_techset::CODE_T6_HDR_CONTROL_1,
            &[float4_bits([1.0, 0.0, 0.0, 0.0])],
        );
        for (index, row) in crate::t6_techset::CODE_T6_REFLECTION_SH
            .into_iter()
            .chain(crate::t6_techset::CODE_T6_GRID_SH)
            .zip([[1.0, 1.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0], [0.0; 4]].repeat(2))
        {
            sources.set_constant_rows(index, &[float4_bits(row)]);
        }
        sources.set_constant_rows(
            crate::t6_techset::CODE_T6_SAMPLE_DECODE,
            &[float4_bits([
                inputs.model_lighting_decode_scale,
                inputs.reflection_probe_alpha_weight,
                0.0,
                0.0,
            ])],
        );
        if let Some(authored) = inputs.sky_intensity {
            let forward_z = inputs.world_from_view.transform_vector3(Vec3::NEG_Z).z;
            produce_sky_constants(sources, authored, forward_z);
        }
        produce_leftover_t5_light_hero_scale(sources);
        produce_leftover_t5_hero_lighting_matrix(sources);

        for index in [
            crate::t5_code_remap::T5_CODE_GENERIC_PARAM0,
            crate::t5_code_remap::T5_CODE_GENERIC_PARAM1,
        ] {
            sources.set_constant_rows(
                crate::t5_code_remap::LEFTOVER_T5_CODE_BASE + index,
                &[[0; 4]],
            );
        }

        sources.set_constant_rows(
            crate::t5_code_remap::LEFTOVER_T5_CODE_BASE
                + crate::t5_code_remap::T5_CODE_EXTRA_CAM_PARAM,
            &[[0; 4]],
        );
        produce_leftover_t5_initial_water_waves(sources, inputs.time);
        produce_leftover_t5_generic_param4(sources);
        produce_leftover_t5_generic_param5(sources);
        produce_leftover_t5_generic_param6(sources);
        produce_leftover_t5_wind_shader_constants(sources);
        produce_leftover_t5_custom_wind_constants(sources);
        produce_leftover_t5_grass_wind_force0(sources);
        produce_leftover_t5_character_charred_amount(sources);
        if let Some(scatter) = inputs.tree_scatter {
            produce_leftover_t5_treecanopy_parms(sources, scatter[0], scatter[1]);
        }

        Ok(())
    }
}

struct BindingWriter<'a> {
    requested: &'a [u16],
    sources: &'a mut RuntimeCodeSources,
}
impl BindingWriter<'_> {
    fn set_constant_rows(&mut self, index: u16, rows: &[[u32; 4]]) {
        if self.requested.binary_search(&index).is_ok() {
            self.sources.set_constant_rows(index, rows);
        }
    }
}
fn float4_bits(row: [f32; 4]) -> [u32; 4] {
    row.map(f32::to_bits)
}
const VIEWPORT_ONE: f32 = 1.0;
const T5_HDRCONTROL_HOST_EXPOSURE: f32 = 1.0;
const CODE_LEFTOVER_IW5_EYEOFFSET: u16 =
    crate::iw5_tech_map::LEFTOVER_IW5_CODE_BASE + crate::iw5_tech_map::IW5_CODE_EYEOFFSET;
const CODE_LEFTOVER_IW5_SAT_R: u16 =
    crate::iw5_tech_map::LEFTOVER_IW5_CODE_BASE + crate::iw5_tech_map::IW5_CODE_COLOR_SATURATION_R;
const CODE_LEFTOVER_IW5_SAT_G: u16 =
    crate::iw5_tech_map::LEFTOVER_IW5_CODE_BASE + crate::iw5_tech_map::IW5_CODE_COLOR_SATURATION_G;
const CODE_LEFTOVER_IW5_SAT_B: u16 =
    crate::iw5_tech_map::LEFTOVER_IW5_CODE_BASE + crate::iw5_tech_map::IW5_CODE_COLOR_SATURATION_B;
const CODE_LEFTOVER_T5_VPOSX: u16 =
    crate::t5_code_remap::LEFTOVER_T5_CODE_BASE + crate::t5_code_remap::T5_CODE_VPOSX_TO_WORLD;
const CODE_LEFTOVER_T5_VPOSY: u16 =
    crate::t5_code_remap::LEFTOVER_T5_CODE_BASE + crate::t5_code_remap::T5_CODE_VPOSY_TO_WORLD;
const CODE_LEFTOVER_T5_VPOS1: u16 =
    crate::t5_code_remap::LEFTOVER_T5_CODE_BASE + crate::t5_code_remap::T5_CODE_VPOS1_TO_WORLD;
const CODE_LEFTOVER_T5_EYEOFFSET: u16 =
    crate::t5_code_remap::LEFTOVER_T5_CODE_BASE + crate::t5_code_remap::T5_CODE_EYEOFFSET;
const CODE_LEFTOVER_T5_SUN_POSITION: u16 =
    crate::t5_code_remap::LEFTOVER_T5_CODE_BASE + crate::t5_code_remap::T5_CODE_SUN_POSITION;
const CODE_LEFTOVER_T5_SUN_DIFFUSE: u16 =
    crate::t5_code_remap::LEFTOVER_T5_CODE_BASE + crate::t5_code_remap::T5_CODE_SUN_DIFFUSE;
const CODE_LEFTOVER_T5_SUN_SPECULAR: u16 =
    crate::t5_code_remap::LEFTOVER_T5_CODE_BASE + crate::t5_code_remap::T5_CODE_SUN_SPECULAR;
const CODE_LEFTOVER_T5_HDRCONTROL_0: u16 =
    crate::t5_code_remap::LEFTOVER_T5_CODE_BASE + crate::t5_code_remap::T5_CODE_HDRCONTROL_0;
const CODE_LEFTOVER_T5_HDRCONTROL_1: u16 =
    crate::t5_code_remap::LEFTOVER_T5_CODE_BASE + crate::t5_code_remap::T5_CODE_HDRCONTROL_1;
const CODE_LEFTOVER_T5_LIGHT_HERO_SCALE: u16 =
    crate::t5_code_remap::LEFTOVER_T5_CODE_BASE + crate::t5_code_remap::T5_CODE_LIGHT_HERO_SCALE;
const CODE_LEFTOVER_T5_HERO_LIGHTING_R: u16 =
    crate::t5_code_remap::LEFTOVER_T5_CODE_BASE + crate::t5_code_remap::T5_CODE_HERO_LIGHTING_R;
const CODE_LEFTOVER_T5_HERO_LIGHTING_G: u16 =
    crate::t5_code_remap::LEFTOVER_T5_CODE_BASE + crate::t5_code_remap::T5_CODE_HERO_LIGHTING_G;
const CODE_LEFTOVER_T5_HERO_LIGHTING_B: u16 =
    crate::t5_code_remap::LEFTOVER_T5_CODE_BASE + crate::t5_code_remap::T5_CODE_HERO_LIGHTING_B;
const CODE_LEFTOVER_T5_GENERIC_PARAM4: u16 =
    crate::t5_code_remap::LEFTOVER_T5_CODE_BASE + crate::t5_code_remap::T5_CODE_GENERIC_PARAM4;
const CODE_LEFTOVER_T5_GENERIC_PARAM5: u16 =
    crate::t5_code_remap::LEFTOVER_T5_CODE_BASE + crate::t5_code_remap::T5_CODE_GENERIC_PARAM5;
const CODE_LEFTOVER_T5_GENERIC_PARAM6: u16 =
    crate::t5_code_remap::LEFTOVER_T5_CODE_BASE + crate::t5_code_remap::T5_CODE_GENERIC_PARAM6;
const CODE_LEFTOVER_T5_WIND_DIRECTION: u16 =
    crate::t5_code_remap::LEFTOVER_T5_CODE_BASE + crate::t5_code_remap::T5_CODE_WIND_DIRECTION;
const CODE_LEFTOVER_T5_GRASS_WIND_FORCE0: u16 =
    crate::t5_code_remap::LEFTOVER_T5_CODE_BASE + crate::t5_code_remap::T5_CODE_GRASS_WIND_FORCE0;
const CODE_LEFTOVER_T5_VARIANT_WIND_SPRING_0: u16 = crate::t5_code_remap::LEFTOVER_T5_CODE_BASE
    + crate::t5_code_remap::T5_CODE_VARIANT_WIND_SPRING_0;
const CODE_LEFTOVER_T5_TREECANOPY_PARMS: u16 =
    crate::t5_code_remap::LEFTOVER_T5_CODE_BASE + crate::t5_code_remap::T5_CODE_TREECANOPY_PARMS;
const CODE_LEFTOVER_T5_CUSTOMWIND_CENTER: u16 =
    crate::t5_code_remap::LEFTOVER_T5_CODE_BASE + crate::t5_code_remap::T5_CODE_CUSTOMWIND_CENTER;
const CODE_LEFTOVER_T5_CUSTOMWIND_SPRING: u16 =
    crate::t5_code_remap::LEFTOVER_T5_CODE_BASE + crate::t5_code_remap::T5_CODE_CUSTOMWIND_SPRING;
const CODE_LEFTOVER_T5_CHARACTER_CHARRED_AMOUNT: u16 = crate::t5_code_remap::LEFTOVER_T5_CODE_BASE
    + crate::t5_code_remap::T5_CODE_CHARACTER_CHARRED_AMOUNT;
const T5_HDRCONTROL_EXPOSURE_DIVISOR: f32 = 8.0;
const R_FILM_TWEAK_SATURATION_DEFAULT: f32 = 1.0;
fn produce_leftover_t5_sun_constants(sources: &mut BindingWriter<'_>, light: &MaterialSunInputs) {
    sources.set_constant_rows(
        CODE_LEFTOVER_T5_SUN_POSITION,
        &[float4_bits(lighting_iw4::dir_light_position(
            light.direction,
        ))],
    );
    sources.set_constant_rows(
        CODE_LEFTOVER_T5_SUN_DIFFUSE,
        &[float4_bits(leftover_t5_sun_color(
            light.diffuse_color,
            light.color,
        ))],
    );
    sources.set_constant_rows(
        CODE_LEFTOVER_T5_SUN_SPECULAR,
        &[float4_bits(leftover_t5_sun_color(
            light.specular_color,
            light.color,
        ))],
    );
}

fn leftover_t5_sun_color(t5: Option<[f32; 4]>, color: [f32; 3]) -> [f32; 4] {
    match t5 {
        Some(v) => [v[0], v[1], v[2], 1.0],
        None => [color[0], color[1], color[2], 1.0],
    }
}

fn produce_leftover_t5_hdrcontrol(sources: &mut BindingWriter<'_>, exposure: f32) {
    sources.set_constant_rows(
        CODE_LEFTOVER_T5_HDRCONTROL_0,
        &[float4_bits([
            exposure / T5_HDRCONTROL_EXPOSURE_DIVISOR,
            0.0,
            0.0,
            0.0,
        ])],
    );
    sources.set_constant_rows(
        CODE_LEFTOVER_T5_HDRCONTROL_1,
        &[float4_bits([1.0, 0.0, 0.0, 0.0])],
    );
}

fn produce_sky_constants(sources: &mut BindingWriter<'_>, authored: [f32; 4], forward_z: f32) {
    for (index, row) in [
        (18, [1.0, 0.0, 0.0, 0.0]),
        (19, [0.0, 1.0, 0.0, 0.0]),
        (20, [0.0, 0.0, 1.0, 0.0]),
    ] {
        sources.set_constant_rows(index, &[float4_bits(row)]);
    }
    sources.set_constant_rows(
        crate::t5_code_remap::LEFTOVER_T5_CODE_BASE + crate::t5_code_remap::T5_CODE_SKY_TRANSITION,
        &[float4_bits([0.0; 4])],
    );
    let intensity = sky_intensity(authored, forward_z);
    sources.set_constant_rows(
        crate::t6_techset::CODE_T6_SKY_COLOR_MULTIPLIER,
        &[float4_bits([intensity; 4])],
    );
    sources.set_constant_rows(
        crate::t5_code_remap::LEFTOVER_T5_CODE_BASE
            + crate::t5_code_remap::T5_CODE_SKY_COLOR_MULTIPLIER,
        &[float4_bits([intensity; 4])],
    );
}

fn sky_intensity([angle0, angle1, factor0, factor1]: [f32; 4], forward_z: f32) -> f32 {
    let radians = f32::from_bits(0x3c8efa35);
    let cos0 = (((90.0 - angle0) * radians) as f64).cos() as f32;
    let cos1 = (((90.0 - angle1) * radians) as f64).cos() as f32;
    let delta = cos1 - cos0;
    let blend = if delta.abs() <= f32::from_bits(0x38d1b717) {
        0.0
    } else {
        let t = ((forward_z - cos0) / delta).clamp(0.0, 1.0);
        t * t
    };
    (1.0 - blend) * factor0 + blend * factor1
}

fn produce_leftover_t5_initial_water_waves(sources: &mut BindingWriter<'_>, time: f32) {
    let wave_number = f32::from_bits(0x40c9_0fdb);
    let gravity = f32::from_bits(0x43c1_1c29);
    let phase = ((wave_number * gravity) as f64).sqrt() * f64::from(time);
    let rows = [
        [wave_number, 0.0, 1.0, 0.0],
        [wave_number, 0.0, 1.0, 0.0],
        [wave_number, 0.0, 1.0, 0.0],
        [wave_number, 0.0, 1.0, 0.0],
        [phase as f32; 4],
        [0.0; 4],
        [0.0; 4],
    ];
    for (row, values) in rows.into_iter().enumerate() {
        sources.set_constant_rows(
            crate::t5_code_remap::LEFTOVER_T5_CODE_BASE
                + crate::t5_code_remap::T5_CODE_POSTFX_CONTROL0
                + row as u16,
            &[float4_bits(values)],
        );
    }
}

fn produce_leftover_t5_light_hero_scale(sources: &mut BindingWriter<'_>) {
    sources.set_constant_rows(
        CODE_LEFTOVER_T5_LIGHT_HERO_SCALE,
        &[float4_bits([1.0, 1.0, 1.0, 1.0])],
    );
}

fn produce_leftover_t5_hero_lighting_matrix(sources: &mut BindingWriter<'_>) {
    sources.set_constant_rows(
        CODE_LEFTOVER_T5_HERO_LIGHTING_R,
        &[float4_bits([1.0, 0.0, 0.0, 0.0])],
    );
    sources.set_constant_rows(
        CODE_LEFTOVER_T5_HERO_LIGHTING_G,
        &[float4_bits([0.0, 1.0, 0.0, 0.0])],
    );
    sources.set_constant_rows(
        CODE_LEFTOVER_T5_HERO_LIGHTING_B,
        &[float4_bits([0.0, 0.0, 1.0, 0.0])],
    );
}

fn produce_leftover_t5_generic_param4(sources: &mut BindingWriter<'_>) {
    sources.set_constant_rows(
        CODE_LEFTOVER_T5_GENERIC_PARAM4,
        &[float4_bits([1.0, 1.0, 1.0, 1.0])],
    );
}

fn produce_leftover_t5_generic_param5(sources: &mut BindingWriter<'_>) {
    sources.set_constant_rows(
        CODE_LEFTOVER_T5_GENERIC_PARAM5,
        &[float4_bits([1.0, 1.0, 1.0, 1.0])],
    );
}

fn produce_leftover_t5_generic_param6(sources: &mut BindingWriter<'_>) {
    sources.set_constant_rows(
        CODE_LEFTOVER_T5_GENERIC_PARAM6,
        &[float4_bits([1.0, 1.0, 1.0, 1.0])],
    );
}

fn produce_leftover_t5_wind_shader_constants(sources: &mut BindingWriter<'_>) {
    sources.set_constant_rows(
        CODE_LEFTOVER_T5_WIND_DIRECTION,
        &[float4_bits([1.0, 0.0, 0.0, 0.0])],
    );
    for index in 0u16..16 {
        sources.set_constant_rows(
            CODE_LEFTOVER_T5_VARIANT_WIND_SPRING_0 + index,
            &[float4_bits([0.0, 0.0, 0.0, 0.0])],
        );
    }
}

fn produce_leftover_t5_custom_wind_constants(sources: &mut BindingWriter<'_>) {
    sources.set_constant_rows(
        CODE_LEFTOVER_T5_CUSTOMWIND_CENTER,
        &[float4_bits([0.0, 0.0, 0.0, 0.0])],
    );
    sources.set_constant_rows(
        CODE_LEFTOVER_T5_CUSTOMWIND_SPRING,
        &[float4_bits([0.0, 0.0, 0.0, 0.0])],
    );
}

fn produce_leftover_t5_character_charred_amount(sources: &mut BindingWriter<'_>) {
    sources.set_constant_rows(
        CODE_LEFTOVER_T5_CHARACTER_CHARRED_AMOUNT,
        &[float4_bits([0.0, 0.0, 0.0, 0.0])],
    );
}

fn produce_leftover_t5_grass_wind_force0(sources: &mut BindingWriter<'_>) {
    sources.set_constant_rows(
        CODE_LEFTOVER_T5_GRASS_WIND_FORCE0,
        &[float4_bits([0.0, 0.0, 0.0, 0.0])],
    );
}

fn produce_leftover_t5_treecanopy_parms(
    sources: &mut BindingWriter<'_>,
    intensity: f32,
    amount: f32,
) {
    sources.set_constant_rows(
        CODE_LEFTOVER_T5_TREECANOPY_PARMS,
        &[float4_bits([intensity, amount, 0.0, 0.0])],
    );
}

fn color_saturation_matrix(saturation: f32) -> [[f32; 4]; 3] {
    let r = (1.0 - saturation) * 0.25;
    let g = (1.0 - saturation) * 0.5;
    [
        [r + saturation, r, r, 0.0],
        [g, g + saturation, g, 0.0],
        [r, r, r + saturation, 0.0],
    ]
}

fn produce_leftover_iw5_code_consts(sources: &mut BindingWriter<'_>, view_origin: Vec3) {
    sources.set_constant_rows(
        CODE_LEFTOVER_IW5_EYEOFFSET,
        &[float4_bits([
            view_origin.x,
            view_origin.y,
            view_origin.z,
            1.0,
        ])],
    );
    let rows = color_saturation_matrix(R_FILM_TWEAK_SATURATION_DEFAULT);
    sources.set_constant_rows(CODE_LEFTOVER_IW5_SAT_R, &[float4_bits(rows[0])]);
    sources.set_constant_rows(CODE_LEFTOVER_IW5_SAT_G, &[float4_bits(rows[1])]);
    sources.set_constant_rows(CODE_LEFTOVER_IW5_SAT_B, &[float4_bits(rows[2])]);
}

fn produce_leftover_t5_code_consts(
    sources: &mut BindingWriter<'_>,
    view_origin: Vec3,
    clip_from_view: Mat4,
    world_from_view: Mat4,
    rt_width: i32,
    rt_height: i32,
) {
    sources.set_constant_rows(
        CODE_LEFTOVER_T5_EYEOFFSET,
        &[float4_bits([
            view_origin.x,
            view_origin.y,
            view_origin.z,
            1.0,
        ])],
    );
    if rt_width <= 0 || rt_height <= 0 {
        return;
    }
    let inv_w = VIEWPORT_ONE / rt_width as f32;
    let inv_h = VIEWPORT_ONE / rt_height as f32;
    let p00 = clip_from_view.x_axis.x;
    let p11 = clip_from_view.y_axis.y;
    if p00.abs() < 1e-12 || p11.abs() < 1e-12 {
        return;
    }
    let scale_x = (-2.0 * inv_w) / p00;
    let scale_y = (2.0 * inv_h) / p11;

    let vposx = world_from_view.x_axis * scale_x;
    let vposy = world_from_view.y_axis * scale_y;
    let vpos1 = world_from_view * Vec4::new(1.0 / p00, -1.0 / p11, 1.0, 0.0);
    sources.set_constant_rows(CODE_LEFTOVER_T5_VPOSX, &[float4_bits(vposx.to_array())]);
    sources.set_constant_rows(CODE_LEFTOVER_T5_VPOSY, &[float4_bits(vposy.to_array())]);
    sources.set_constant_rows(CODE_LEFTOVER_T5_VPOS1, &[float4_bits(vpos1.to_array())]);
}

pub fn fog_color_linear_and_gamma(rgb: [f32; 3], alpha: f32) -> ([f32; 4], [f32; 4]) {
    let pack = |c: f32| -> u8 { (c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8 };

    let bytes = [pack(rgb[2]), pack(rgb[1]), pack(rgb[0]), pack(alpha)];
    const INV_255: f32 = 0.003_921_568_9;
    let gamma = [
        f32::from(bytes[2]) * INV_255,
        f32::from(bytes[1]) * INV_255,
        f32::from(bytes[0]) * INV_255,
        f32::from(bytes[3]) * INV_255,
    ];
    let linear = [
        lighting_iw4::color_srgb_to_linear(gamma[0]),
        lighting_iw4::color_srgb_to_linear(gamma[1]),
        lighting_iw4::color_srgb_to_linear(gamma[2]),
        gamma[3],
    ];
    (linear, gamma)
}
