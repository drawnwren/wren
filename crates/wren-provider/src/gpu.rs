use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use wgpu::util::DeviceExt as _;

const WORKGROUP_SIZE: u32 = 256;
const MAP_TIMEOUT: Duration = Duration::from_secs(5);

struct Buffers {
    input: wgpu::Buffer,
    output: wgpu::Buffer,
    readback: Option<wgpu::Buffer>,
    bind_group: wgpu::BindGroup,
    input_capacity: u64,
    output_capacity: u64,
}

#[derive(Clone, Copy)]
struct DispatchSize {
    input: usize,
    output: usize,
    workgroups_x: u32,
    workgroups_y: u32,
    invocations_per_row: u32,
}

impl DispatchSize {
    fn new(text_len: usize, max_workgroups: u32) -> Option<Self> {
        let input = input_size(text_len)?;
        let output = output_size(text_len)?;
        let (workgroups_x, workgroups_y) = dispatch_dimensions(text_len, max_workgroups)?;
        Some(Self { input, output, workgroups_x, workgroups_y, invocations_per_row: workgroups_x.checked_mul(WORKGROUP_SIZE)? })
    }

    fn fits(self, max_buffer_size: u64) -> bool {
        [self.input, self.output].into_iter().all(|size| u64::try_from(size).is_ok_and(|size| size <= max_buffer_size))
    }
}

pub(super) struct GpuLexical {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    params: wgpu::Buffer,
    buffers: Option<Buffers>,
    max_buffer_size: u64,
    max_workgroups: u32,
    direct_readback: bool,
    failed: Arc<AtomicBool>,
    upload: Vec<u8>,
}

impl GpuLexical {
    pub(super) fn new() -> Result<Self, String> {
        let mut instance_options = wgpu::InstanceDescriptor::new_without_display_handle();
        instance_options.backends = wgpu::Backends::METAL;
        let instance = wgpu::Instance::new(instance_options);
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
            apply_limit_buckets: false,
        }))
        .map_err(|error| format!("request GPU adapter: {error}"))?;
        let adapter_info = adapter.get_info();
        if matches!(adapter_info.device_type, wgpu::DeviceType::Cpu | wgpu::DeviceType::Other) {
            return Err(format!("adapter {} is not a hardware GPU", adapter_info.name));
        }
        let direct_readback =
            adapter_info.device_type == wgpu::DeviceType::IntegratedGpu && adapter.features().contains(wgpu::Features::MAPPABLE_PRIMARY_BUFFERS);
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("wren-provider-compute"),
            required_features: if direct_readback { wgpu::Features::MAPPABLE_PRIMARY_BUFFERS } else { wgpu::Features::empty() },
            ..Default::default()
        }))
        .map_err(|error| format!("request GPU device: {error}"))?;
        let failed = Arc::new(AtomicBool::new(false));
        let callback_failed = Arc::clone(&failed);
        device.on_uncaptured_error(Arc::new(move |_| {
            callback_failed.store(true, Ordering::Release);
        }));

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("wren lexical classifier"),
            source: wgpu::ShaderSource::Wgsl(include_str!("lexical.wgsl").into()),
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("wren lexical classifier"),
            layout: None,
            module: &shader,
            entry_point: Some("classify"),
            compilation_options: Default::default(),
            cache: None,
        });
        device.poll(wgpu::PollType::Poll).map_err(|error| format!("poll GPU after pipeline creation: {error}"))?;
        if failed.load(Ordering::Acquire) {
            return Err("GPU rejected the lexical compute pipeline".to_owned());
        }

        let params = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("wren lexical parameters"),
            contents: &[0; 16],
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let limits = device.limits();
        Ok(Self {
            device,
            queue,
            pipeline,
            params,
            buffers: None,
            max_buffer_size: limits.max_storage_buffer_binding_size,
            max_workgroups: limits.max_compute_workgroups_per_dimension,
            direct_readback,
            failed,
            upload: Vec::new(),
        })
    }

    pub(super) fn supports(&self, text_len: usize) -> bool {
        text_len > 0 && u32::try_from(text_len).is_ok() && DispatchSize::new(text_len, self.max_workgroups).is_some_and(|size| size.fits(self.max_buffer_size))
    }

    pub(super) fn classify(&mut self, text: &str, upload_required: bool) -> Result<Vec<Range<usize>>, String> {
        let size = self.validate_dispatch(text)?;
        let buffers_replaced =
            self.ensure_capacity(u64::try_from(size.input).map_err(|error| error.to_string())?, u64::try_from(size.output).map_err(|error| error.to_string())?);
        if upload_required || buffers_replaced {
            self.upload_text(text, size)?;
        }
        let buffers = self.buffers.as_ref().ok_or_else(|| "GPU buffers were not initialized".to_owned())?;
        let submission = self.submit(buffers, size)?;
        self.read_ranges(buffers, submission, size.output, text)
    }

    fn validate_dispatch(&self, text: &str) -> Result<DispatchSize, String> {
        if !text.is_ascii() {
            return Err("GPU lexical classifier currently requires ASCII input".to_owned());
        }
        if !self.supports(text.len()) {
            return Err("document exceeds GPU lexical classifier limits".to_owned());
        }
        if self.failed.load(Ordering::Acquire) {
            return Err("GPU lexical classifier device is unavailable".to_owned());
        }
        DispatchSize::new(text.len(), self.max_workgroups).ok_or_else(|| "document exceeds GPU dispatch limits".to_owned())
    }

    fn upload_text(&mut self, text: &str, size: DispatchSize) -> Result<(), String> {
        let buffers = self.buffers.as_ref().ok_or_else(|| "GPU buffers were not initialized".to_owned())?;
        let input = if size.input == text.len() {
            text.as_bytes()
        } else {
            self.upload.resize(size.input, 0);
            self.upload[..text.len()].copy_from_slice(text.as_bytes());
            &self.upload
        };
        self.queue.write_buffer(&buffers.input, 0, input);
        let mut params = [0_u8; 16];
        params[..4].copy_from_slice(&u32::try_from(text.len()).map_err(|error| error.to_string())?.to_le_bytes());
        params[4..8].copy_from_slice(&size.invocations_per_row.to_le_bytes());
        self.queue.write_buffer(&self.params, 0, &params);
        Ok(())
    }

    fn submit(&self, buffers: &Buffers, size: DispatchSize) -> Result<wgpu::SubmissionIndex, String> {
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("wren lexical classifier") });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("wren lexical classifier"), timestamp_writes: None });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &buffers.bind_group, &[]);
            pass.dispatch_workgroups(size.workgroups_x, size.workgroups_y, 1);
        }
        if let Some(readback) = &buffers.readback {
            encoder.copy_buffer_to_buffer(&buffers.output, 0, readback, 0, u64::try_from(size.output).map_err(|error| error.to_string())?);
        }
        Ok(self.queue.submit([encoder.finish()]))
    }

    fn read_ranges(&self, buffers: &Buffers, submission: wgpu::SubmissionIndex, output_size: usize, text: &str) -> Result<Vec<Range<usize>>, String> {
        let readback = buffers.readback.as_ref().unwrap_or(&buffers.output);
        let slice = readback.slice(..u64::try_from(output_size).map_err(|error| error.to_string())?);
        let (sender, receiver) = mpsc::sync_channel(1);
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.device
            .poll(wgpu::PollType::Wait { submission_index: Some(submission), timeout: Some(MAP_TIMEOUT) })
            .map_err(|error| format!("wait for GPU lexical classifier: {error}"))?;
        receiver
            .recv_timeout(MAP_TIMEOUT)
            .map_err(|error| format!("receive GPU lexical classifier result: {error}"))?
            .map_err(|error| format!("map GPU lexical classifier result: {error}"))?;
        if self.failed.load(Ordering::Acquire) {
            readback.unmap();
            return Err("GPU lexical classifier reported a device error".to_owned());
        }
        let result =
            slice.get_mapped_range().map_err(|error| format!("read GPU lexical classifier result: {error}")).map(|mapped| decode_ranges(&mapped, text));
        readback.unmap();
        result
    }

    fn ensure_capacity(&mut self, input_required: u64, output_required: u64) -> bool {
        if self.buffers.as_ref().is_some_and(|buffers| buffers.input_capacity >= input_required && buffers.output_capacity >= output_required) {
            return false;
        }
        let input_capacity = input_required.checked_next_power_of_two().filter(|capacity| *capacity <= self.max_buffer_size).unwrap_or(input_required);
        let output_capacity = output_required.checked_next_power_of_two().filter(|capacity| *capacity <= self.max_buffer_size).unwrap_or(output_required);
        let input = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("wren lexical input"),
            size: input_capacity,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let output = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("wren lexical output"),
            size: output_capacity,
            usage: wgpu::BufferUsages::STORAGE | if self.direct_readback { wgpu::BufferUsages::MAP_READ } else { wgpu::BufferUsages::COPY_SRC },
            mapped_at_creation: false,
        });
        let readback = (!self.direct_readback).then(|| {
            self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("wren lexical readback"),
                size: output_capacity,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            })
        });
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("wren lexical classifier"),
            layout: &self.pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: input.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: output.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: self.params.as_entire_binding() },
            ],
        });
        self.buffers = Some(Buffers { input, output, readback, bind_group, input_capacity, output_capacity });
        true
    }
}

fn decode_ranges(mapped: &[u8], text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    for (word_index, bytes) in mapped.chunks_exact(4).enumerate() {
        let mut starts = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        while starts != 0 {
            let bit = usize::try_from(starts.trailing_zeros()).unwrap_or(0);
            let start = word_index * 32 + bit;
            if start < text.len()
                && let Some(length) = keyword_length_at(text.as_bytes(), start)
            {
                ranges.push(start..start + length);
            }
            starts &= starts - 1;
        }
    }
    ranges
}

fn input_size(length: usize) -> Option<usize> {
    length.checked_add(3).map(|length| length & !3)
}

fn output_size(length: usize) -> Option<usize> {
    length.checked_add(31).and_then(|length| (length / 32).checked_mul(4))
}

fn workgroup_size() -> usize {
    usize::try_from(WORKGROUP_SIZE).unwrap_or(256)
}

fn dispatch_dimensions(text_len: usize, max_workgroups: u32) -> Option<(u32, u32)> {
    let total = text_len.div_ceil(workgroup_size());
    let maximum = usize::try_from(max_workgroups).ok()?;
    if total == 0 || maximum == 0 {
        return None;
    }
    let y = total.div_ceil(maximum);
    if y > maximum {
        return None;
    }
    let x = total.div_ceil(y);
    Some((u32::try_from(x).ok()?, u32::try_from(y).ok()?))
}

fn keyword_length_at(text: &[u8], start: usize) -> Option<usize> {
    [b"fn".as_slice(), b"let", b"mut", b"struct", b"enum", b"impl", b"trait", b"pub", b"use", b"match", b"if", b"else", b"for", b"while", b"return"]
        .into_iter()
        .find(|keyword| text.get(start..start + keyword.len()) == Some(*keyword))
        .map(<[u8]>::len)
}

#[cfg(test)]
mod tests {
    use super::{WORKGROUP_SIZE, dispatch_dimensions};

    #[test]
    fn two_dimensional_dispatch_has_at_most_one_partial_row() {
        let maximum = 65_535;
        let total_workgroups = maximum * 2 + 2;
        let text_len = usize::try_from(total_workgroups).unwrap_or(usize::MAX).saturating_mul(usize::try_from(WORKGROUP_SIZE).unwrap_or(256));

        let (x, y) = dispatch_dimensions(text_len, maximum).expect("valid dispatch");
        let dispatched = u64::from(x) * u64::from(y);

        assert!(x <= maximum);
        assert!(y <= maximum);
        assert!(dispatched >= u64::from(total_workgroups));
        assert!(dispatched - u64::from(total_workgroups) < u64::from(y));
    }
}
