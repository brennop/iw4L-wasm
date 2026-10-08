use super::*;

pub(super) struct Iw4Compiler;

impl MaterialCompiler for Iw4Compiler {
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
