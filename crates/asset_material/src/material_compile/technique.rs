use super::program_hash;
use crate::{MaterialDefinitions, OwnedTechnique};
use asset_core::AssetNamespace;
use render_material::{
    PassColorSpace, RuntimeArgumentBinding, RuntimePass, RuntimeShaderPair, RuntimeShaderProgramId,
    RuntimeShaderStage, RuntimeTechnique, RuntimeVertexDecl, sort_pass_args,
};

pub(super) trait MaterialCompiler {
    fn draw_rules(
        &self,
        material: &crate::AuthoredMaterial,
        _unlit: bool,
    ) -> render_material::MaterialDrawRules {
        render_material::MaterialDrawRules {
            colour_camera_region: material.camera_region,
            smodel_colour_emits: true,
            unlit_sky: false,
            postfx_host_supported: material.namespace == AssetNamespace::Iw4,
        }
    }
    fn compile_state(&self, words: [u32; 2]) -> render_material::CompiledPassState;
    fn color_space(&self, slot: u8) -> PassColorSpace;
    fn hardware_shadow_compare(&self) -> bool;

    fn compile_technique(
        &self,
        source: &MaterialDefinitions,
        slot: u8,
        technique: &OwnedTechnique,
        vertex_decls: &mut Vec<(u32, RuntimeVertexDecl)>,
    ) -> RuntimeTechnique {
        let passes = technique
            .passes
            .iter()
            .map(|pass| {
                let shader = |reference: &crate::OwnedShaderRef| {
                    reference
                        .shader
                        .and_then(|index| source.shaders.get(index))
                        .and_then(|shader| {
                            u32::try_from(reference.shader?).ok().map(|asset_slot| {
                                RuntimeShaderProgramId {
                                    asset_slot,
                                    program_hash: program_hash(&shader.program),
                                }
                            })
                        })
                };

                let vertex_decl_slot = pass
                    .vertex_decl
                    .and_then(|id| u32::try_from(id).ok())
                    .unwrap_or(u32::MAX);
                if let Some(authored) = pass
                    .vertex_decl
                    .and_then(|index| source.vertex_decls.get(index))
                {
                    let record = RuntimeVertexDecl {
                        family: authored.family,
                        name: authored.name.to_string(),
                        stream_count: authored.stream_count,
                        has_optional_source: authored.has_optional_source,
                        routing: authored.routing,
                    };
                    match vertex_decls
                        .binary_search_by_key(&vertex_decl_slot, |(identity, _)| *identity)
                    {
                        Ok(_) => {}
                        Err(at) => vertex_decls.insert(at, (vertex_decl_slot, record)),
                    }
                }
                let shader_pair = shader(&pass.vertex_shader)
                    .zip(shader(&pass.pixel_shader))
                    .map(|(vertex, pixel)| RuntimeShaderPair {
                        vertex,
                        pixel,
                        vertex_decl_slot,
                    });
                let arguments = pass
                    .arguments
                    .iter()
                    .map(|argument| match argument {
                        crate::OwnedShaderArgument::MaterialVertexConstant {
                            destination,
                            name_hash,
                        } => RuntimeArgumentBinding::MaterialConstant {
                            stage: RuntimeShaderStage::Vertex,
                            destination: *destination,
                            name_hash: *name_hash,
                        },
                        crate::OwnedShaderArgument::LiteralVertexConstant {
                            destination,
                            words,
                        } => RuntimeArgumentBinding::LiteralConstant {
                            stage: RuntimeShaderStage::Vertex,
                            destination: *destination,
                            words: *words,
                        },
                        crate::OwnedShaderArgument::MaterialPixelSampler {
                            destination,
                            name_hash,
                        } => RuntimeArgumentBinding::MaterialTexture {
                            destination: *destination,
                            name_hash: *name_hash,
                        },
                        crate::OwnedShaderArgument::CodeVertexConstant {
                            destination,
                            index,
                            first_row,
                            row_count,
                        } => RuntimeArgumentBinding::CodeConstant {
                            stage: RuntimeShaderStage::Vertex,
                            destination: *destination,
                            index: *index,
                            first_row: *first_row,
                            row_count: *row_count,
                        },
                        crate::OwnedShaderArgument::CodePixelSampler { destination, index } => {
                            RuntimeArgumentBinding::CodeTexture {
                                destination: *destination,
                                index: *index,
                            }
                        }
                        crate::OwnedShaderArgument::CodePixelConstant {
                            destination,
                            index,
                            first_row,
                            row_count,
                        } => RuntimeArgumentBinding::CodeConstant {
                            stage: RuntimeShaderStage::Pixel,
                            destination: *destination,
                            index: *index,
                            first_row: *first_row,
                            row_count: *row_count,
                        },
                        crate::OwnedShaderArgument::MaterialPixelConstant {
                            destination,
                            name_hash,
                        } => RuntimeArgumentBinding::MaterialConstant {
                            stage: RuntimeShaderStage::Pixel,
                            destination: *destination,
                            name_hash: *name_hash,
                        },
                        crate::OwnedShaderArgument::LiteralPixelConstant { destination, words } => {
                            RuntimeArgumentBinding::LiteralConstant {
                                stage: RuntimeShaderStage::Pixel,
                                destination: *destination,
                                words: *words,
                            }
                        }
                        crate::OwnedShaderArgument::Unknown { argument_type, raw } => {
                            RuntimeArgumentBinding::Unknown {
                                argument_type: *argument_type,
                                raw: *raw,
                            }
                        }
                    })
                    .collect();
                let mut runtime_pass = RuntimePass {
                    shader_pair,
                    custom_sampler_flags: pass.custom_sampler_flags,
                    t5_custom_sampler_flags: pass.t5_custom_sampler_flags,
                    per_prim_arg_count: pass.per_prim_arg_count,
                    per_obj_arg_count: pass.per_obj_arg_count,
                    stable_arg_count: pass.stable_arg_count,
                    arguments,
                    color_space: self.color_space(slot),
                    hardware_shadow_compare: self.hardware_shadow_compare(),
                };

                sort_pass_args(&mut runtime_pass);
                runtime_pass
            })
            .collect();
        RuntimeTechnique {
            source_selection: technique.source_selection,
            flags: technique.flags,
            passes,
        }
    }
}

struct LinearCompiler;
struct GammaCompiler;
struct DxbcCompiler;

impl MaterialCompiler for LinearCompiler {
    fn compile_state(&self, words: [u32; 2]) -> render_material::CompiledPassState {
        render_material::compile_material_state(AssetNamespace::Iw4, words)
    }
    fn color_space(&self, _slot: u8) -> PassColorSpace {
        PassColorSpace::Linear
    }
    fn hardware_shadow_compare(&self) -> bool {
        false
    }
}

impl MaterialCompiler for GammaCompiler {
    fn draw_rules(
        &self,
        material: &crate::AuthoredMaterial,
        _unlit: bool,
    ) -> render_material::MaterialDrawRules {
        render_material::MaterialDrawRules {
            colour_camera_region: if material.camera_region == 3 {
                asset_iw4::CAMERA_REGION_NONE
            } else {
                material.camera_region
            },
            smodel_colour_emits: fastfile_t5::state_bits::smodel_camera_emits(
                material.info_game_flags,
                material.camera_region,
            ),
            unlit_sky: false,
            postfx_host_supported: false,
        }
    }
    fn compile_state(&self, words: [u32; 2]) -> render_material::CompiledPassState {
        render_material::compile_material_state(AssetNamespace::T5, words)
    }
    fn color_space(&self, slot: u8) -> PassColorSpace {
        if lighting_iw4::is_lit_remap_slot(slot) {
            PassColorSpace::GammaEncoded
        } else {
            PassColorSpace::Unknown
        }
    }
    fn hardware_shadow_compare(&self) -> bool {
        true
    }
}

impl MaterialCompiler for DxbcCompiler {
    fn draw_rules(
        &self,
        material: &crate::AuthoredMaterial,
        unlit: bool,
    ) -> render_material::MaterialDrawRules {
        render_material::MaterialDrawRules {
            colour_camera_region: material.camera_region,
            smodel_colour_emits: true,
            unlit_sky: unlit
                && matches!(
                    material.sort_key,
                    asset_iw4::SORT_KEY_SKY | asset_iw4::SORT_KEY_SKYBOX
                ),
            postfx_host_supported: false,
        }
    }
    fn compile_state(&self, words: [u32; 2]) -> render_material::CompiledPassState {
        render_material::compile_material_state(AssetNamespace::T6, words)
    }
    fn color_space(&self, _slot: u8) -> PassColorSpace {
        PassColorSpace::Unknown
    }
    fn hardware_shadow_compare(&self) -> bool {
        false
    }
}

pub(super) fn compiler_for(namespace: AssetNamespace) -> &'static dyn MaterialCompiler {
    match namespace {
        AssetNamespace::Iw4 | AssetNamespace::Iw5 => &LinearCompiler,
        AssetNamespace::T5 => &GammaCompiler,
        AssetNamespace::T6 => &DxbcCompiler,
    }
}
