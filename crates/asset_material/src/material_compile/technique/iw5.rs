use super::*;

pub(super) struct Iw5Compiler;

impl MaterialCompiler for Iw5Compiler {
    fn compile_state(&self, words: [u32; 2]) -> render_material::CompiledPassState {
        iw4::Iw4Compiler.compile_state(words)
    }
    fn color_space(&self, slot: u8) -> PassColorSpace {
        iw4::Iw4Compiler.color_space(slot)
    }
    fn hardware_shadow_compare(&self) -> bool {
        false
    }
}
