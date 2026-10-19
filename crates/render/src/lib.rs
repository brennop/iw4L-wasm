pub mod diag;
mod extract;
mod plugin;
mod resolution;

pub use plugin::RenderPlugin;
pub use render_gpu::{ColourWorkingSet, WorldPipelineWarmup, web_profile};
