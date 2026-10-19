use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::{Arc, Weak};

use bevy::prelude::Resource;
use bevy::render::render_resource::*;
use bevy::render::renderer::{PendingCommandBuffers, RenderDevice, RenderQueue};
use render_frame::skinning::{PackedSkinJob, PackedSkinShape};

use super::residency::GpuStream;

const VERTEX_BYTES: u64 = 32;
const PARAMS_BYTES: u64 = 48;

struct Pipeline {
    layout: BindGroupLayout,
    pipeline: ComputePipeline,
}

/// Lives as long as the main world holds the shape, not as long as the shape
/// keeps showing up in frames: a body culled for one frame keeps its upload.
struct ResidentShape {
    owner: Weak<PackedSkinShape>,
    buffer: Buffer,
    binding: Option<([BufferId; 3], BindGroup)>,
}

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Skinned {
    pub(super) jobs: usize,
    pub(super) vertices: usize,
}

#[derive(Resource, Default)]
pub(super) struct SkinGpu {
    pipeline: Option<Pipeline>,
    shapes: HashMap<u64, ResidentShape>,
    reported: HashMap<u64, Weak<PackedSkinShape>>,
    bones: GpuStream,
    params: GpuStream,
    bone_rows: Vec<[[f32; 4]; 4]>,
    param_rows: Vec<u8>,
    accepted: Vec<usize>,
    refused: Vec<(usize, &'static str)>,
}

impl SkinGpu {
    pub(super) fn resident_bytes(&self) -> u64 {
        self.shapes
            .values()
            .map(|shape| shape.buffer.size())
            .chain(
                [self.bones.buffer(), self.params.buffer()]
                    .into_iter()
                    .flatten()
                    .map(|buffer| buffer.size()),
            )
            .sum()
    }

    pub(super) fn dispatch(
        &mut self,
        jobs: &[PackedSkinJob],
        output: Option<&Buffer>,
        vertex_count: usize,
        device: &RenderDevice,
        queue: &RenderQueue,
        pending: &mut PendingCommandBuffers,
    ) -> Skinned {
        let _prepare = perf::Span::RenderSkinPrepareMs.enter();
        self.shapes
            .retain(|_, shape| shape.owner.strong_count() != 0);
        self.reported.retain(|_, shape| shape.strong_count() != 0);
        self.accepted.clear();
        self.refused.clear();
        if jobs.is_empty() {
            return Skinned::default();
        }
        let Some(output) = output else {
            self.refused
                .extend((0..jobs.len()).map(|index| (index, "no xmodel vertex buffer")));
            self.report(jobs);
            return Skinned::default();
        };
        let limits = device.limits();
        let max_binding = limits.max_storage_buffer_binding_size;
        let bank = (vertex_count as u64).min(output.size() / VERTEX_BYTES) as usize;
        let bank_refusal = (vertex_count as u64 * VERTEX_BYTES > output.size()
            || output.size() > max_binding)
            .then_some("xmodel vertex bank exceeds its buffer or the storage binding limit");
        for (index, job) in jobs.iter().enumerate() {
            if job.ranges.iter().all(|range| range.count == 0) {
                continue;
            }
            let count = job.shape.vertices().len();
            let end = (job.destination as usize).checked_add(count);
            let refusal = bank_refusal.or(if count == 0 {
                Some("empty shape")
            } else if end.is_none_or(|end| end > vertex_count) {
                Some("destination outside the xmodel vertex bank")
            } else if count.div_ceil(64) > limits.max_compute_workgroups_per_dimension as usize
                || count as u64 * 64 > max_binding
            {
                Some("shape exceeds the dispatch or binding limit")
            } else if job.ranges.iter().any(|range| {
                range
                    .start
                    .checked_add(range.count)
                    .is_none_or(|end| end as usize > count)
            }) {
                Some("surface range outside the shape")
            } else if job.bones.is_empty() || job.shape.required_bones() > job.bones.len() {
                Some("bone palette shorter than the shape")
            } else {
                None
            });
            match refusal {
                Some(reason) => self.refused.push((index, reason)),
                None => self.accepted.push(index),
            }
        }
        self.accepted
            .sort_unstable_by_key(|&index| jobs[index].destination);
        let mut covered = 0usize;
        let refused = &mut self.refused;
        self.accepted.retain(|&index| {
            let start = jobs[index].destination as usize;
            if start < covered {
                refused.push((index, "overlaps another job's destination"));
                return false;
            }
            covered = start + jobs[index].shape.vertices().len();
            true
        });
        let stride =
            PARAMS_BYTES.next_multiple_of(u64::from(limits.min_uniform_buffer_offset_alignment));
        self.bone_rows.clear();
        self.param_rows.clear();
        let mut kept = Vec::with_capacity(self.accepted.len());
        for &index in &self.accepted {
            let job = &jobs[index];
            let bone_base = self.bone_rows.len();
            if (bone_base + job.bones.len()) as u64 * 64 > max_binding {
                self.refused
                    .push((index, "frame bone palette exceeds the binding limit"));
                continue;
            }
            self.bone_rows.extend_from_slice(&job.bones);
            for range in job.ranges.iter().filter(|range| range.count != 0) {
                let params = [
                    job.destination,
                    range.count,
                    bone_base as u32,
                    range.start,
                    job.offset[0].to_bits(),
                    job.offset[1].to_bits(),
                    job.offset[2].to_bits(),
                    0,
                    u32::from(range.rigid),
                    0,
                    0,
                    0,
                ];
                let slot = self.param_rows.len().div_ceil(stride as usize);
                self.param_rows.resize(slot * stride as usize, 0);
                self.param_rows
                    .extend_from_slice(bytemuck::cast_slice(&params));
            }
            kept.push(index);
        }
        self.accepted = kept;
        if !self.accepted.is_empty() {
            self.bones.reserve(
                device,
                "packed_skin_bones",
                BufferUsages::STORAGE,
                self.bone_rows.len(),
                64,
            );
            self.params.reserve(
                device,
                "packed_skin_params",
                BufferUsages::UNIFORM,
                self.param_rows.len().div_ceil(stride as usize),
                stride as usize,
            );
            if !(self
                .bones
                .write_at(queue, 0, bytemuck::cast_slice(&self.bone_rows))
                && self.params.write_at(queue, 0, &self.param_rows))
            {
                self.refused.extend(
                    self.accepted
                        .drain(..)
                        .map(|index| (index, "bone or parameter upload refused")),
                );
            }
        }
        self.report(jobs);
        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor {
            label: Some("packed_skin"),
        });
        for &(index, _) in &self.refused {
            let job = &jobs[index];
            let start = (job.destination as usize).min(bank);
            let end = start.saturating_add(job.shape.vertices().len()).min(bank);
            if end > start {
                encoder.clear_buffer(
                    output,
                    start as u64 * VERTEX_BYTES,
                    Some((end - start) as u64 * VERTEX_BYTES),
                );
            }
        }
        let mut skinned = Skinned::default();
        if !self.accepted.is_empty() {
            let pipeline = self.pipeline.get_or_insert_with(|| make_pipeline(device));
            let bones = self.bones.buffer().expect("bones reserved");
            let params = self.params.buffer().expect("params reserved");
            let key = [bones.id(), params.id(), output.id()];
            let mut pass = encoder.begin_compute_pass(&ComputePassDescriptor {
                label: Some("packed_skin"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&pipeline.pipeline);
            let mut slot = 0u64;
            for &index in &self.accepted {
                let job = &jobs[index];
                let shape = self
                    .shapes
                    .entry(job.shape.id())
                    .or_insert_with(|| ResidentShape {
                        owner: Arc::downgrade(&job.shape),
                        buffer: device.create_buffer_with_data(&BufferInitDescriptor {
                            label: Some("packed_skin_shape"),
                            contents: bytemuck::cast_slice(job.shape.vertices()),
                            usage: BufferUsages::STORAGE,
                        }),
                        binding: None,
                    });
                if shape.binding.as_ref().is_none_or(|(at, _)| *at != key) {
                    let binding = device.create_bind_group(
                        "packed_skin",
                        &pipeline.layout,
                        &BindGroupEntries::sequential((
                            shape.buffer.as_entire_binding(),
                            bones.as_entire_binding(),
                            BindingResource::Buffer(BufferBinding {
                                buffer: params,
                                offset: 0,
                                size: NonZeroU64::new(PARAMS_BYTES),
                            }),
                            output.as_entire_binding(),
                        )),
                    );
                    shape.binding = Some((key, binding));
                }
                let (_, binding) = shape.binding.as_ref().expect("binding installed");
                for range in job.ranges.iter().filter(|range| range.count != 0) {
                    pass.set_bind_group(0, &**binding, &[(slot * stride) as u32]);
                    pass.dispatch_workgroups(range.count.div_ceil(64), 1, 1);
                    slot += 1;
                    skinned.vertices += range.count as usize;
                }
                skinned.jobs += 1;
            }
        }
        // Pending buffers precede the render graph's colour and shadow consumers.
        pending.push([encoder.finish()]);
        skinned
    }

    fn report(&mut self, jobs: &[PackedSkinJob]) {
        for &(index, reason) in &self.refused {
            let shape = &jobs[index].shape;
            if self.reported.contains_key(&shape.id()) {
                continue;
            }
            self.reported.insert(shape.id(), Arc::downgrade(shape));
            diag::error!(
                World,
                "packed skinning hid job {index} (shape {}, {} vertices at {}): {reason}",
                shape.id(),
                shape.vertices().len(),
                jobs[index].destination,
            );
        }
    }
}

fn make_pipeline(device: &RenderDevice) -> Pipeline {
    let storage = |binding, read_only| BindGroupLayoutEntry {
        binding,
        visibility: ShaderStages::COMPUTE,
        ty: BindingType::Buffer {
            ty: BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    };
    let layout = device.create_bind_group_layout(
        "packed_skin",
        &[
            storage(0, true),
            storage(1, true),
            BindGroupLayoutEntry {
                binding: 2,
                visibility: ShaderStages::COMPUTE,
                ty: BindingType::Buffer {
                    ty: BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: NonZeroU64::new(PARAMS_BYTES),
                },
                count: None,
            },
            storage(3, false),
        ],
    );
    let module = device.create_and_validate_shader_module(ShaderModuleDescriptor {
        label: Some("packed_skin"),
        source: ShaderSource::Wgsl(include_str!("skinning.wgsl").into()),
    });
    let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
        label: Some("packed_skin"),
        bind_group_layouts: &[Some(&*layout)],
        immediate_size: 0,
    });
    let pipeline = device.create_compute_pipeline(&RawComputePipelineDescriptor {
        label: Some("packed_skin"),
        layout: Some(&pipeline_layout),
        module: &module,
        entry_point: Some("skin_main"),
        compilation_options: Default::default(),
        cache: None,
    });
    Pipeline { layout, pipeline }
}
