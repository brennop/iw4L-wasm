use std::sync::{Arc, Mutex};

use bevy::mesh::VertexBufferLayout;
use bevy::platform::collections::HashMap;
use bevy::prelude::Resource;
use bevy::render::render_resource::{
    BindGroupLayout, BindGroupLayoutDescriptor, ColorTargetState, DepthStencilState,
    MultisampleState, PipelineCompilationOptions, PipelineLayout, PipelineLayoutDescriptor,
    PrimitiveState, RawFragmentState, RawRenderPipelineDescriptor, RawVertexBufferLayout,
    RawVertexState, RenderPipeline, ShaderModule, ShaderModuleDescriptor, ShaderSource,
};
use bevy::render::renderer::RenderDevice;
use bevy::tasks::{Task, futures_lite::future};

use super::sm3_wgsl::{PASS_VERTEX_ENTRY, ValidatedPassWgsl};
use render_material::PortId;

/// How much pipeline creation one `flush` may start.
///
/// The browser default keeps each frame's share of module creation and
/// descriptor conversion small so frames still present during the load; the
/// pipelines themselves compile asynchronously there. Native is unbounded. `IW4L_EXACT_MODULES_PER_FRAME` and
/// `IW4L_EXACT_PIPELINES_PER_FRAME` override either (0 = unbounded). A call
/// always takes at least one module, and stops once either bound is reached,
/// so a single module's plans may exceed the pipeline bound.
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

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct ModuleKey {
    port: PortId,
    cached_lighting: bool,
}

#[derive(Clone)]
pub(super) enum ExactModuleSource {
    Port(Arc<ValidatedPassWgsl>),
    CachedLighting(Arc<str>),
}

impl ExactModuleSource {
    fn wgsl(&self) -> &str {
        match self {
            Self::Port(module) => &module.source,
            Self::CachedLighting(source) => source,
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

struct PortBuild {
    module: ModuleKey,
    created: Option<Arc<ShaderModule>>,
    pipelines: Vec<(ExactPipelineSlot, RenderPipeline)>,
}

#[derive(Resource, Default)]
pub(super) struct ExactPipelineRegistry {
    slots: Vec<SlotState>,
    by_key: HashMap<super::colour_submit::ExactColourPipelineKey, ExactPipelineSlot>,
    modules: HashMap<ModuleKey, Arc<ShaderModule>>,

    queued: HashMap<
        ModuleKey,
        (
            ExactModuleSource,
            Vec<(ExactPipelineSlot, ExactPipelinePlan)>,
        ),
    >,
    jobs: Vec<Task<PortBuild>>,

    layouts: Mutex<HashMap<BindGroupLayoutDescriptor, BindGroupLayout>>,
    pipeline_layouts: HashMap<(BindGroupLayout, BindGroupLayout), PipelineLayout>,

    discovered: Mutex<Vec<super::colour_submit::ExactColourPipelineKey>>,
}

impl ExactPipelineRegistry {
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
        source: ExactModuleSource,
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

        let module = ModuleKey {
            port: key.port,
            cached_lighting: matches!(source, ExactModuleSource::CachedLighting(_)),
        };
        self.queued
            .entry(module)
            .or_insert_with(|| (source, Vec::new()))
            .1
            .push((slot, plan));
        slot
    }

    /// Starts a bounded slice of the queued work; the rest stays in `queued`
    /// (its slots stay `Building`, so warm-up counts it as not ready) and the
    /// caller flushes again next frame. Wasm runs spawned futures as
    /// microtasks, so an unbounded drain is one multi-second burst with no
    /// frame presented.
    pub(super) fn flush(&mut self, device: &RenderDevice) {
        if self.queued.is_empty() {
            return;
        }

        let budget = FlushBudget::get();
        let pool = assets::load_pool();
        let mut modules = 0usize;
        let mut pipelines = 0usize;
        while modules < budget.modules && pipelines < budget.pipelines {
            let Some(module) = self.queued.keys().next().copied() else {
                break;
            };
            let (source, plans) = self
                .queued
                .remove(&module)
                .expect("key was just read from the queue");
            modules += 1;
            pipelines += plans.len();

            let plans: Vec<_> = plans
                .into_iter()
                .map(|(slot, plan)| {
                    let layout = self.pipeline_layout(device, &plan);
                    (slot, plan, layout)
                })
                .collect();
            let device = device.clone();
            let existing = self.modules.get(&module).cloned();
            self.jobs.push(
                pool.spawn(
                    async move { build_port(&device, module, source, existing, plans).await },
                ),
            );
        }
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

    /// Modules or plans still waiting for a later `flush`.
    pub(super) fn queued_n(&self) -> usize {
        self.queued.values().map(|(_, plans)| plans.len()).sum()
    }

    pub(super) fn poll(&mut self) {
        let mut index = 0;
        while index < self.jobs.len() {
            let Some(build) = future::block_on(future::poll_once(&mut self.jobs[index])) else {
                index += 1;
                continue;
            };

            drop(self.jobs.swap_remove(index));
            if let Some(created) = build.created {
                self.modules.insert(build.module, created);
            }
            for (slot, pipeline) in build.pipelines {
                self.slots[slot.0 as usize] = SlotState::Ready(pipeline);
            }
        }
    }

    pub(super) fn module_n(&self) -> usize {
        self.modules.len()
    }

    pub(super) fn building_n(&self) -> usize {
        self.jobs.len()
    }
}

async fn build_port(
    device: &RenderDevice,
    module: ModuleKey,
    source: ExactModuleSource,
    existing: Option<Arc<ShaderModule>>,
    plans: Vec<(ExactPipelineSlot, ExactPipelinePlan, PipelineLayout)>,
) -> PortBuild {
    let label = format!(
        "iw4_exact_colour/{:016x}/{}{}",
        module.port.vertex_program_hash,
        module.port.vertex_type,
        if module.cached_lighting {
            "/cached"
        } else {
            ""
        }
    );

    let (shader, created) = match existing {
        Some(shader) => (shader, None),
        None => {
            let shader = Arc::new(unsafe {
                device.create_shader_module(ShaderModuleDescriptor {
                    label: Some(&label),
                    source: ShaderSource::Wgsl(std::borrow::Cow::Borrowed(source.wgsl())),
                })
            });
            (shader.clone(), Some(shader))
        }
    };
    // A browser compiles each stage of an async pipeline independently and
    // caches the result, but only for pipelines started after it resolved.
    // Starting a whole port at once therefore compiles the shared vertex and
    // fragment stages once per pipeline. So the port goes in three waves, each
    // awaited before the next: one pipeline per layout (vertex stage plus one
    // fragment stage), then one per remaining (layout, fragment entry), then
    // the rest, which differ only in render state and hit the cache for both
    // stages. Each wave still runs concurrently off the browser's GPU main
    // thread (`createRenderPipelineAsync`). Native creates synchronously here.
    let mut seen_layouts = Vec::new();
    let mut seen_entries = Vec::new();
    let mut waves: [Vec<_>; 3] = Default::default();
    for item in plans {
        let layout = (
            item.1.constants_layout.clone(),
            item.1.textures_layout.clone(),
        );
        let entry = (layout.clone(), item.1.fragment_entry.clone());
        let wave = if !seen_layouts.contains(&layout) {
            seen_layouts.push(layout);
            seen_entries.push(entry);
            0
        } else if !seen_entries.contains(&entry) {
            seen_entries.push(entry);
            1
        } else {
            2
        };
        waves[wave].push(item);
    }
    let mut pipelines = Vec::new();
    for wave in waves {
        let started: Vec<_> = wave
            .iter()
            .map(|(_, plan, layout)| {
                with_pipeline_descriptor(&shader, plan, layout, |descriptor| {
                    device
                        .wgpu_device()
                        .create_render_pipeline_async(descriptor)
                })
            })
            .collect();
        for ((slot, plan, layout), pipeline) in wave.into_iter().zip(started) {
            let pipeline = match pipeline.await {
                Ok(pipeline) => RenderPipeline::from(pipeline),
                Err(error) => {
                    // The sync call reports the same error through the
                    // device's error handling, as before async creation.
                    bevy::log::error!("exact pipeline {}: {error}", plan.label);
                    with_pipeline_descriptor(&shader, &plan, &layout, |descriptor| {
                        device.create_render_pipeline(descriptor)
                    })
                }
            };
            pipelines.push((slot, pipeline));
        }
    }
    PortBuild {
        module,
        created,
        pipelines,
    }
}

fn with_pipeline_descriptor<R>(
    shader: &ShaderModule,
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
            module: shader,
            entry_point: Some(PASS_VERTEX_ENTRY),
            buffers: &buffers,
            compilation_options: compilation_options.clone(),
        },
        fragment: Some(RawFragmentState {
            module: shader,
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
