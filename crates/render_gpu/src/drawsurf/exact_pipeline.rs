use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};

use bevy::mesh::VertexBufferLayout;
use bevy::platform::collections::{HashMap, HashSet};
use bevy::prelude::Resource;
use bevy::render::render_resource::{
    BindGroupLayout, BindGroupLayoutDescriptor, ColorTargetState, DepthStencilState,
    MultisampleState, PipelineCompilationOptions, PipelineLayout, PipelineLayoutDescriptor,
    PrimitiveState, RawFragmentState, RawRenderPipelineDescriptor, RawVertexBufferLayout,
    RawVertexState, RenderPipeline, ShaderModule, ShaderModuleDescriptor, ShaderSource,
};
use bevy::render::renderer::RenderDevice;
use bevy::tasks::{Task, futures_lite::future};

use super::sm3_wgsl::PASS_VERTEX_ENTRY;

/// How much pipeline creation one `flush` may start.
///
/// The browser default keeps each frame's share of module creation and
/// descriptor conversion small so frames still present during the load; the
/// pipelines themselves compile asynchronously there. Native is unbounded. `IW4L_EXACT_MODULES_PER_FRAME` and
/// `IW4L_EXACT_PIPELINES_PER_FRAME` override either (0 = unbounded). "Modules" counts
/// shader modules created for the first time. A call always starts at least one
/// pipeline, and stops once either bound is reached, so a single pipeline that
/// needs two new modules may exceed the module bound.
#[derive(Clone, Copy)]
struct FlushBudget {
    modules: usize,
    pipelines: usize,
}

impl FlushBudget {
    const WEB: Self = Self {
        modules: 16,
        pipelines: 64,
    };

    fn get() -> Self {
        static BUDGET: std::sync::OnceLock<FlushBudget> = std::sync::OnceLock::new();
        *BUDGET.get_or_init(|| {
            let knob = |name: &str, default: usize| {
                std::env::var(name)
                    .ok()
                    .and_then(|value| value.parse::<usize>().ok())
                    .map_or(default, |n| if n == 0 { usize::MAX } else { n })
            };
            let default = if cfg!(target_arch = "wasm32") {
                Self::WEB
            } else {
                Self {
                    modules: usize::MAX,
                    pipelines: usize::MAX,
                }
            };
            Self {
                modules: knob("IW4L_EXACT_MODULES_PER_FRAME", default.modules),
                pipelines: knob("IW4L_EXACT_PIPELINES_PER_FRAME", default.pipelines),
            }
        })
    }
}

/// The WGSL text of one shader module. It is the module's identity: equal text is one
/// module, whichever port, stage or variant asked for it. The hash is taken once, so a
/// key lookup does not rehash the text.
#[derive(Clone)]
pub(super) struct ModuleText {
    text: Arc<str>,
    hash: u64,
}

impl ModuleText {
    pub(super) fn new(text: Arc<str>) -> Self {
        let hash = std::hash::BuildHasher::hash_one(&bevy::platform::hash::FixedHasher, &*text);
        Self { text, hash }
    }
}

impl PartialEq for ModuleText {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.text, &other.text) || (self.hash == other.hash && self.text == other.text)
    }
}

impl Eq for ModuleText {}

impl Hash for ModuleText {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.hash);
    }
}

/// The two module texts of one pipeline. A texture-table port has one combined module, so
/// both stages hold the same text.
#[derive(Clone)]
pub(super) struct ExactModuleSource {
    pub(super) vertex: ModuleText,
    pub(super) fragment: ModuleText,
}

impl ExactModuleSource {
    pub(super) fn new(vertex: Arc<str>, fragment: Arc<str>) -> Self {
        let vertex = ModuleText::new(vertex);
        let fragment = if Arc::ptr_eq(&vertex.text, &fragment) {
            vertex.clone()
        } else {
            ModuleText::new(fragment)
        };
        Self { vertex, fragment }
    }

    /// The same program with its vertex entry replaced. A combined module serves both
    /// stages, so the replacement serves both too; a split fragment module stays shared.
    pub(super) fn with_vertex(&self, vertex: Arc<str>) -> Self {
        if self.vertex == self.fragment {
            Self::new(vertex.clone(), vertex)
        } else {
            Self {
                vertex: ModuleText::new(vertex),
                fragment: self.fragment.clone(),
            }
        }
    }
}

pub(super) struct ExactPipelinePlan {
    pub(super) label: String,
    pub(super) fragment_entry: String,
    pub(super) vertex_buffers: Vec<VertexBufferLayout>,
    pub(super) targets: Vec<Option<ColorTargetState>>,
    pub(super) primitive: PrimitiveState,
    pub(super) depth_stencil: DepthStencilState,
    pub(super) multisample: MultisampleState,
    pub(super) constants_layout: BindGroupLayout,
    pub(super) textures_layout: BindGroupLayout,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub(super) struct ExactPipelineSlot(u32);

enum SlotState {
    Building,
    Ready(RenderPipeline),
}

type LayoutKey = (BindGroupLayout, BindGroupLayout);

/// What a browser caches for a vertex stage: the module and the pipeline layout (which
/// sets the binding remap). Entry point and fragment inputs are fixed.
#[derive(Clone, PartialEq, Eq, Hash)]
struct VertexStageKey {
    module: ModuleText,
    layout: LayoutKey,
}

/// What a browser caches for a fragment stage.
#[derive(Clone, PartialEq, Eq, Hash)]
struct FragmentStageKey {
    module: ModuleText,
    entry: String,
    layout: LayoutKey,
}

type ModuleCell = Arc<OnceLock<Arc<ShaderModule>>>;

struct QueuedPipeline {
    slot: ExactPipelineSlot,
    plan: ExactPipelinePlan,
    modules: ExactModuleSource,
    vertex_key: VertexStageKey,
    fragment_key: FragmentStageKey,
}

struct PipelineBuild {
    slot: ExactPipelineSlot,
    pipeline: RenderPipeline,
    vertex_key: VertexStageKey,
    fragment_key: FragmentStageKey,
}

/// Pipeline creation, scheduled by stage.
///
/// A browser compiles each stage of an async pipeline independently and caches the
/// result, but only for pipelines started after it resolved. Pipelines that share a stage
/// therefore must not start together. A pipeline starts only when each of its stages is
/// warm (a pipeline using it has resolved) or is introduced by this pipeline alone; one
/// that needs a stage another in-flight pipeline is introducing stays queued, and starts
/// on the first `flush` after that pipeline resolves. This holds across ports, since
/// ports share stage modules, and keeps the users of one module close together in time.
#[derive(Resource, Default)]
pub(super) struct ExactPipelineRegistry {
    world: frame::WorldGeneration,
    slots: Vec<SlotState>,
    by_key: HashMap<super::colour_submit::ExactColourPipelineKey, ExactPipelineSlot>,
    modules: HashMap<ModuleText, ModuleCell>,

    queued: Vec<QueuedPipeline>,
    jobs: Vec<Task<PipelineBuild>>,
    warm_vertex: HashSet<VertexStageKey>,
    warm_fragment: HashSet<FragmentStageKey>,
    introducing_vertex: HashSet<VertexStageKey>,
    introducing_fragment: HashSet<FragmentStageKey>,
    stage_counts_logged: bool,

    layouts: Mutex<HashMap<BindGroupLayoutDescriptor, BindGroupLayout>>,
    pipeline_layouts: HashMap<(BindGroupLayout, BindGroupLayout), PipelineLayout>,

    discovered: Mutex<Vec<super::colour_submit::ExactColourPipelineKey>>,
}

impl ExactPipelineRegistry {
    pub(super) fn set_world(&mut self, world: frame::WorldGeneration) -> bool {
        if self.world == world {
            return false;
        }
        *self = Self {
            world,
            ..Self::default()
        };
        true
    }

    pub(super) fn bind_group_layout(
        &self,
        device: &RenderDevice,
        descriptor: &BindGroupLayoutDescriptor,
    ) -> BindGroupLayout {
        self.layouts
            .lock()
            .expect("exact pipeline layout cache is never poisoned")
            .entry(descriptor.clone())
            .or_insert_with_key(|descriptor| {
                device.create_bind_group_layout(descriptor.label.as_ref(), &descriptor.entries)
            })
            .clone()
    }

    pub(super) fn discover(&self, key: super::colour_submit::ExactColourPipelineKey) {
        self.discovered
            .lock()
            .expect("exact pipeline discovery list is never poisoned")
            .push(key);
    }

    pub(super) fn take_discovered(&mut self) -> Vec<super::colour_submit::ExactColourPipelineKey> {
        std::mem::take(
            &mut *self
                .discovered
                .lock()
                .expect("exact pipeline discovery list is never poisoned"),
        )
    }

    pub(super) fn slot(
        &self,
        key: &super::colour_submit::ExactColourPipelineKey,
    ) -> Option<ExactPipelineSlot> {
        self.by_key.get(key).copied()
    }

    pub(super) fn ready(&self, slot: ExactPipelineSlot) -> Option<&RenderPipeline> {
        match self.slots.get(slot.0 as usize) {
            Some(SlotState::Ready(pipeline)) => Some(pipeline),
            _ => None,
        }
    }

    pub(super) fn is_ready(&self, slot: ExactPipelineSlot) -> bool {
        matches!(self.slots.get(slot.0 as usize), Some(SlotState::Ready(_)))
    }

    pub(super) fn request(
        &mut self,
        key: super::colour_submit::ExactColourPipelineKey,
        modules: ExactModuleSource,
        plan: ExactPipelinePlan,
    ) -> ExactPipelineSlot {
        if let Some(slot) = self.by_key.get(&key).copied() {
            return slot;
        }
        let slot = ExactPipelineSlot(
            u32::try_from(self.slots.len()).expect("exact pipeline slot count fits u32"),
        );
        self.slots.push(SlotState::Building);
        self.by_key.insert(key, slot);

        let layout = (plan.constants_layout.clone(), plan.textures_layout.clone());
        let vertex_key = VertexStageKey {
            module: modules.vertex.clone(),
            layout: layout.clone(),
        };
        let fragment_key = FragmentStageKey {
            module: modules.fragment.clone(),
            entry: plan.fragment_entry.clone(),
            layout,
        };
        self.queued.push(QueuedPipeline {
            slot,
            plan,
            modules,
            vertex_key,
            fragment_key,
        });
        slot
    }

    /// Starts a bounded slice of the queued pipelines that no in-flight pipeline blocks;
    /// the rest stays in `queued` (its slots stay `Building`, so warm-up counts it as not
    /// ready) and the caller flushes again next frame. Wasm runs spawned futures as
    /// microtasks, so an unbounded drain is one multi-second burst with no frame presented.
    pub(super) fn flush(&mut self, device: &RenderDevice) {
        if self.queued.is_empty() {
            return;
        }

        let budget = FlushBudget::get();
        let pool = assets::load_pool();
        let mut new_modules = 0usize;
        let mut started = 0usize;
        let queued = std::mem::take(&mut self.queued);
        let mut waiting = Vec::with_capacity(queued.len());
        for item in queued {
            if new_modules >= budget.modules
                || started >= budget.pipelines
                || self.introducing_vertex.contains(&item.vertex_key)
                || self.introducing_fragment.contains(&item.fragment_key)
            {
                waiting.push(item);
                continue;
            }
            if !self.warm_vertex.contains(&item.vertex_key) {
                self.introducing_vertex.insert(item.vertex_key.clone());
            }
            if !self.warm_fragment.contains(&item.fragment_key) {
                self.introducing_fragment.insert(item.fragment_key.clone());
            }
            started += 1;

            let vertex_module = self.module_cell(&item.modules.vertex, &mut new_modules);
            let fragment_module = self.module_cell(&item.modules.fragment, &mut new_modules);
            let layout = self.pipeline_layout(device, &item.plan);
            let device = device.clone();
            self.jobs.push(pool.spawn(async move {
                build_pipeline(&device, item, vertex_module, fragment_module, layout).await
            }));
        }
        self.queued = waiting;
    }

    fn module_cell(&mut self, text: &ModuleText, new_modules: &mut usize) -> ModuleCell {
        self.modules
            .entry(text.clone())
            .or_insert_with(|| {
                *new_modules += 1;
                ModuleCell::default()
            })
            .clone()
    }

    fn pipeline_layout(
        &mut self,
        device: &RenderDevice,
        plan: &ExactPipelinePlan,
    ) -> PipelineLayout {
        self.pipeline_layouts
            .entry((plan.constants_layout.clone(), plan.textures_layout.clone()))
            .or_insert_with(|| {
                device.create_pipeline_layout(&PipelineLayoutDescriptor {
                    label: Some("iw4_exact_colour/layout"),
                    bind_group_layouts: &[
                        Some(&plan.constants_layout),
                        Some(&plan.textures_layout),
                    ],
                    immediate_size: 0,
                })
            })
            .clone()
    }

    /// Pipelines still waiting for a later `flush`.
    pub(super) fn queued_n(&self) -> usize {
        self.queued.len()
    }

    pub(super) fn poll(&mut self) {
        let mut index = 0;
        while index < self.jobs.len() {
            let Some(build) = future::block_on(future::poll_once(&mut self.jobs[index])) else {
                index += 1;
                continue;
            };

            drop(self.jobs.swap_remove(index));
            self.slots[build.slot.0 as usize] = SlotState::Ready(build.pipeline);
            self.introducing_vertex.remove(&build.vertex_key);
            self.warm_vertex.insert(build.vertex_key);
            self.introducing_fragment.remove(&build.fragment_key);
            self.warm_fragment.insert(build.fragment_key);
        }
    }

    /// Shader modules created (or being created) so far.
    pub(super) fn module_n(&self) -> usize {
        self.modules
            .values()
            .filter(|cell| cell.get().is_some())
            .count()
    }

    /// Pipelines being built.
    pub(super) fn building_n(&self) -> usize {
        self.jobs.len()
    }

    /// Logs how many distinct modules and stage keys the resolved pipelines used, the
    /// first time it is called. Call it when warm-up first reaches ready == total.
    pub(super) fn log_stage_counts_once(&mut self) {
        if std::mem::replace(&mut self.stage_counts_logged, true) {
            return;
        }
        let vertex_modules: HashSet<&ModuleText> =
            self.warm_vertex.iter().map(|key| &key.module).collect();
        let fragment_modules: HashSet<&ModuleText> =
            self.warm_fragment.iter().map(|key| &key.module).collect();
        diag::info!(
            World,
            "exact pipelines: stage counts vertex modules={} fragment modules={} vertex stage keys={} fragment stage keys={} pipelines={}",
            vertex_modules.len(),
            fragment_modules.len(),
            self.warm_vertex.len(),
            self.warm_fragment.len(),
            self.slots.len()
        );
    }
}

fn shader_module(device: &RenderDevice, cell: &ModuleCell, text: &ModuleText) -> Arc<ShaderModule> {
    cell.get_or_init(|| {
        Arc::new(unsafe {
            device.create_shader_module(ShaderModuleDescriptor {
                label: Some(&format!("iw4_exact_colour/module/{:016x}", text.hash)),
                source: ShaderSource::Wgsl(std::borrow::Cow::Borrowed(&text.text)),
            })
        })
    })
    .clone()
}

async fn build_pipeline(
    device: &RenderDevice,
    item: QueuedPipeline,
    vertex_module: ModuleCell,
    fragment_module: ModuleCell,
    layout: PipelineLayout,
) -> PipelineBuild {
    let vertex = shader_module(device, &vertex_module, &item.modules.vertex);
    let fragment = shader_module(device, &fragment_module, &item.modules.fragment);
    // Each wave of the old per-port scheduler awaited its pipeline here; now the registry
    // holds back pipelines that share an unresolved stage. The browser creates the
    // pipeline off its GPU main thread (`createRenderPipelineAsync`); native creates it
    // synchronously.
    let pipeline =
        with_pipeline_descriptor(&vertex, &fragment, &item.plan, &layout, |descriptor| {
            device
                .wgpu_device()
                .create_render_pipeline_async(descriptor)
        })
        .await;
    let pipeline = match pipeline {
        Ok(pipeline) => RenderPipeline::from(pipeline),
        Err(error) => {
            // The sync call reports the same error through the
            // device's error handling, as before async creation.
            bevy::log::error!("exact pipeline {}: {error}", item.plan.label);
            with_pipeline_descriptor(&vertex, &fragment, &item.plan, &layout, |descriptor| {
                device.create_render_pipeline(descriptor)
            })
        }
    };
    PipelineBuild {
        slot: item.slot,
        pipeline,
        vertex_key: item.vertex_key,
        fragment_key: item.fragment_key,
    }
}

fn with_pipeline_descriptor<R>(
    vertex: &ShaderModule,
    fragment: &ShaderModule,
    plan: &ExactPipelinePlan,
    layout: &PipelineLayout,
    create: impl FnOnce(&RawRenderPipelineDescriptor) -> R,
) -> R {
    let buffers: Vec<RawVertexBufferLayout> = plan
        .vertex_buffers
        .iter()
        .map(|buffer| RawVertexBufferLayout {
            array_stride: buffer.array_stride,
            attributes: &buffer.attributes,
            step_mode: buffer.step_mode,
        })
        .collect();
    let compilation_options = PipelineCompilationOptions {
        constants: &[],
        zero_initialize_workgroup_memory: false,
    };
    create(&RawRenderPipelineDescriptor {
        label: Some(&plan.label),
        layout: Some(layout),
        vertex: RawVertexState {
            module: vertex,
            entry_point: Some(PASS_VERTEX_ENTRY),
            buffers: &buffers,
            compilation_options: compilation_options.clone(),
        },
        fragment: Some(RawFragmentState {
            module: fragment,
            entry_point: Some(&plan.fragment_entry),
            targets: &plan.targets,
            compilation_options,
        }),
        primitive: plan.primitive,
        depth_stencil: Some(plan.depth_stencil.clone()),
        multisample: plan.multisample,
        multiview_mask: None,
        cache: None,
    })
}
