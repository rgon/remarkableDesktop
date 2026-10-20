//! Edge detection's per-pixel stages (blur, Sobel, suppression, hysteresis,
//! line placement) on the GPU through Vulkan, so a frame costs the CPU only
//! a copy in, a copy out and the fill (see `edges.rs`, which also has the
//! CPU version of everything here). `libvulkan` is loaded at run time: where
//! there is no Vulkan driver, `Gpu::new` fails and the CPU does the work.
//!
//! One compute pipeline per stage of `edges.wgsl`, all sharing one
//! descriptor set of nine storage buffers. The work is recorded once into
//! three command buffers (setup and the first hysteresis rounds; more
//! hysteresis rounds; placement and read-back) and submitted for each frame.
//! Must be created and used on one thread.

use ash::{Device, Entry, Instance, vk};
use std::ffi::CStr;

/// The shader, as built by build.rs.
const SPIRV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/edges.spv"));

/// Entry points of the shader, in `Stage` order.
const STAGES: [&CStr; 8] = [c"blur_h", c"blur_v", c"sobel", c"nms", c"hyst", c"place_line", c"dark_px", c"pack_out"];

#[derive(Clone, Copy)]
enum Stage {
    BlurH,
    BlurV,
    Sobel,
    Nms,
    Hyst,
    Place,
    Dark,
    Pack,
}

/// Hysteresis rounds per submission, and submissions at most. A round
/// extends edges by one pixel, so this covers weak edge chains of 512 pixels
/// (a longer one just stays shorter than on the CPU).
const ROUNDS_PER_BATCH: usize = 8;
const MAX_BATCHES: usize = 64;

/// Storage buffer bindings of `edges.wgsl`.
const INPUT: usize = 0;
const OUTLINE: usize = 5;
const CHANGED: usize = 6;
const RESULT: usize = 7;
const PARAMS: usize = 8;
const BUFFERS: usize = 9;

struct Buf {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    /// Where the CPU sees it (null for buffers only the GPU touches).
    ptr: *mut u8,
}

pub struct Gpu {
    _entry: Entry,
    instance: Instance,
    device: Device,
    queue: vk::Queue,
    pool: vk::CommandPool,
    set_layout: vk::DescriptorSetLayout,
    desc_pool: vk::DescriptorPool,
    pipe_layout: vk::PipelineLayout,
    module: vk::ShaderModule,
    pipelines: Vec<vk::Pipeline>,
    buffers: Vec<Buf>,
    /// Setup, more hysteresis, placement.
    cmds: [vk::CommandBuffer; 3],
    fence: vk::Fence,
    size: (usize, usize),
    name: String,
}

fn ve<T>(what: &str, r: Result<T, vk::Result>) -> Result<T, String> {
    r.map_err(|e| format!("vulkan {what}: {e}"))
}

fn find_memory(props: &vk::PhysicalDeviceMemoryProperties, bits: u32, want: vk::MemoryPropertyFlags) -> Option<u32> {
    (0..props.memory_type_count).find(|&i| bits & (1 << i) != 0 && props.memory_types[i as usize].property_flags.contains(want))
}

impl Gpu {
    /// Opens the best GPU and prepares for frames of `w`×`h`. Pixels of the
    /// frame darker than `dark` come out black (0: none do).
    pub fn new(w: usize, h: usize, dark: u32) -> Result<Gpu, String> {
        let spirv = ash::util::read_spv(&mut std::io::Cursor::new(SPIRV)).map_err(|e| format!("shader: {e}"))?;
        let entry = unsafe { Entry::load() }.map_err(|e| format!("no Vulkan loader: {e}"))?;
        let app = vk::ApplicationInfo::default().api_version(vk::make_api_version(0, 1, 0, 0));
        let instance = ve("instance", unsafe { entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app), None) })?;
        // From here on a failure leaks what was created so far: the caller
        // goes on without a GPU, once.
        let (phys, family, name) = Self::pick(&instance)?;
        let mem_props = unsafe { instance.get_physical_device_memory_properties(phys) };

        let priorities = [1.0f32];
        let queues = [vk::DeviceQueueCreateInfo::default().queue_family_index(family).queue_priorities(&priorities)];
        let device = ve("device", unsafe { instance.create_device(phys, &vk::DeviceCreateInfo::default().queue_create_infos(&queues), None) })?;
        let queue = unsafe { device.get_device_queue(family, 0) };

        // Buffers: the frame in, the outline out, the rest stays on the GPU.
        let px = (w * h) as u64;
        let packed = px.div_ceil(4).max(1) * 4;
        let words = (px * 4).max(4);
        // (size, host access) per binding.
        let layout = [
            (packed, true),
            (words, false),
            (words, false),
            (words, false),
            (words, false),
            (words, false),
            (4, true),
            (packed, true),
            (16, true),
        ];
        let mut buffers = Vec::with_capacity(BUFFERS);
        for (size, host) in layout {
            buffers.push(Self::buffer(&device, &mem_props, size, host)?);
        }
        unsafe {
            let params = buffers[PARAMS].ptr as *mut u32;
            params.write(w as u32);
            params.add(1).write(h as u32);
            params.add(2).write(dark);
        }

        let bindings: Vec<_> = (0..BUFFERS as u32)
            .map(|b| vk::DescriptorSetLayoutBinding::default().binding(b).descriptor_type(vk::DescriptorType::STORAGE_BUFFER).descriptor_count(1).stage_flags(vk::ShaderStageFlags::COMPUTE))
            .collect();
        let set_layout = ve("descriptor layout", unsafe { device.create_descriptor_set_layout(&vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings), None) })?;
        let pool_sizes = [vk::DescriptorPoolSize::default().ty(vk::DescriptorType::STORAGE_BUFFER).descriptor_count(BUFFERS as u32)];
        let desc_pool = ve("descriptor pool", unsafe { device.create_descriptor_pool(&vk::DescriptorPoolCreateInfo::default().max_sets(1).pool_sizes(&pool_sizes), None) })?;
        let set_layouts = [set_layout];
        let set = ve("descriptor set", unsafe { device.allocate_descriptor_sets(&vk::DescriptorSetAllocateInfo::default().descriptor_pool(desc_pool).set_layouts(&set_layouts)) })?[0];
        let infos: Vec<[vk::DescriptorBufferInfo; 1]> = buffers.iter().map(|b| [vk::DescriptorBufferInfo::default().buffer(b.buffer).offset(0).range(vk::WHOLE_SIZE)]).collect();
        let writes: Vec<_> = infos
            .iter()
            .enumerate()
            .map(|(b, info)| vk::WriteDescriptorSet::default().dst_set(set).dst_binding(b as u32).descriptor_type(vk::DescriptorType::STORAGE_BUFFER).buffer_info(info))
            .collect();
        unsafe { device.update_descriptor_sets(&writes, &[]) };

        let pipe_layout = ve("pipeline layout", unsafe { device.create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts), None) })?;
        let module = ve("shader module", unsafe { device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&spirv), None) })?;
        let pipeline_infos: Vec<_> = STAGES
            .iter()
            .map(|name| vk::ComputePipelineCreateInfo::default().stage(vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::COMPUTE).module(module).name(name)).layout(pipe_layout))
            .collect();
        let pipelines = unsafe { device.create_compute_pipelines(vk::PipelineCache::null(), &pipeline_infos, None) }.map_err(|(_, e)| format!("vulkan pipelines: {e}"))?;

        let pool = ve("command pool", unsafe { device.create_command_pool(&vk::CommandPoolCreateInfo::default().queue_family_index(family), None) })?;
        let cmds = ve("command buffers", unsafe { device.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).level(vk::CommandBufferLevel::PRIMARY).command_buffer_count(3)) })?;
        let fence = ve("fence", unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) })?;

        let gpu = Gpu { _entry: entry, instance, device, queue, pool, set_layout, desc_pool, pipe_layout, module, pipelines, buffers, cmds: [cmds[0], cmds[1], cmds[2]], fence, size: (w, h), name };
        gpu.record(set)?;
        Ok(gpu)
    }

    /// The device's name, for the log.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The frame size this was set up for.
    pub fn size(&self) -> (usize, usize) {
        self.size
    }

    /// A GPU (not a software renderer) with a compute queue: discrete first.
    fn pick(instance: &Instance) -> Result<(vk::PhysicalDevice, u32, String), String> {
        let mut best: Option<(u32, vk::PhysicalDevice, u32, String)> = None;
        for phys in ve("devices", unsafe { instance.enumerate_physical_devices() })? {
            let props = unsafe { instance.get_physical_device_properties(phys) };
            let rank = match props.device_type {
                vk::PhysicalDeviceType::DISCRETE_GPU => 3,
                vk::PhysicalDeviceType::INTEGRATED_GPU => 2,
                vk::PhysicalDeviceType::VIRTUAL_GPU => 1,
                _ => continue,
            };
            let families = unsafe { instance.get_physical_device_queue_family_properties(phys) };
            let Some(family) = families.iter().position(|f| f.queue_flags.contains(vk::QueueFlags::COMPUTE)) else { continue };
            if best.as_ref().is_none_or(|b| rank > b.0) {
                let name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }.to_string_lossy().into_owned();
                best = Some((rank, phys, family as u32, name));
            }
        }
        best.map(|(_, p, f, n)| (p, f, n)).ok_or_else(|| "no GPU with compute support".to_string())
    }

    /// A storage buffer; `host`: visible to the CPU (mapped), else GPU-local.
    fn buffer(device: &Device, mem_props: &vk::PhysicalDeviceMemoryProperties, size: u64, host: bool) -> Result<Buf, String> {
        let info = vk::BufferCreateInfo::default().size(size).usage(vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST).sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = ve("buffer", unsafe { device.create_buffer(&info, None) })?;
        let req = unsafe { device.get_buffer_memory_requirements(buffer) };
        let coherent = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        let wanted: &[vk::MemoryPropertyFlags] = if host {
            // Cached memory reads back much faster where there is a choice.
            &[coherent | vk::MemoryPropertyFlags::HOST_CACHED, coherent]
        } else {
            &[vk::MemoryPropertyFlags::DEVICE_LOCAL, vk::MemoryPropertyFlags::empty()]
        };
        let index = wanted.iter().find_map(|&w| find_memory(mem_props, req.memory_type_bits, w)).ok_or("no suitable GPU memory")?;
        let memory = ve("memory", unsafe { device.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(index), None) })?;
        ve("bind", unsafe { device.bind_buffer_memory(buffer, memory, 0) })?;
        let ptr = if host { ve("map", unsafe { device.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()) })? as *mut u8 } else { std::ptr::null_mut() };
        Ok(Buf { buffer, memory, ptr })
    }

    /// Records the three command buffers.
    fn record(&self, set: vk::DescriptorSet) -> Result<(), String> {
        let d = &self.device;
        let groups = (self.size.0.div_ceil(16) as u32, self.size.1.div_ceil(16) as u32);
        let shader = vk::PipelineStageFlags::COMPUTE_SHADER;
        let transfer = vk::PipelineStageFlags::TRANSFER;
        // Everything a stage wrote is there for the next one (and the fills).
        let sync = |cb: vk::CommandBuffer| unsafe {
            let b = vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::TRANSFER_WRITE);
            d.cmd_pipeline_barrier(cb, shader | transfer, shader | transfer, vk::DependencyFlags::empty(), &[b], &[], &[]);
        };
        let to_host = |cb: vk::CommandBuffer| unsafe {
            let b = vk::MemoryBarrier::default().src_access_mask(vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::TRANSFER_WRITE).dst_access_mask(vk::AccessFlags::HOST_READ);
            d.cmd_pipeline_barrier(cb, shader | transfer, vk::PipelineStageFlags::HOST, vk::DependencyFlags::empty(), &[b], &[], &[]);
        };
        let run = |cb: vk::CommandBuffer, stage: Stage| unsafe {
            d.cmd_bind_pipeline(cb, vk::PipelineBindPoint::COMPUTE, self.pipelines[stage as usize]);
            d.cmd_dispatch(cb, groups.0, groups.1, 1);
            sync(cb);
        };
        // Rounds of hysteresis, each reporting in `changed` only whether it changed something.
        let rounds = |cb: vk::CommandBuffer| unsafe {
            for _ in 0..ROUNDS_PER_BATCH {
                d.cmd_fill_buffer(cb, self.buffers[CHANGED].buffer, 0, vk::WHOLE_SIZE, 0);
                sync(cb);
                run(cb, Stage::Hyst);
            }
        };
        for (i, &cb) in self.cmds.iter().enumerate() {
            unsafe {
                ve("begin", d.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default()))?;
                d.cmd_bind_descriptor_sets(cb, vk::PipelineBindPoint::COMPUTE, self.pipe_layout, 0, &[set], &[]);
                match i {
                    0 => {
                        d.cmd_fill_buffer(cb, self.buffers[OUTLINE].buffer, 0, vk::WHOLE_SIZE, 0xffff_ffff);
                        sync(cb);
                        for stage in [Stage::BlurH, Stage::BlurV, Stage::Sobel, Stage::Nms] {
                            run(cb, stage);
                        }
                        rounds(cb);
                    }
                    1 => rounds(cb),
                    _ => {
                        run(cb, Stage::Place);
                        run(cb, Stage::Dark);
                        run(cb, Stage::Pack);
                    }
                }
                to_host(cb);
                ve("end", d.end_command_buffer(cb))?;
            }
        }
        Ok(())
    }

    fn submit(&self, i: usize) -> Result<(), String> {
        let cbs = [self.cmds[i]];
        unsafe {
            ve("submit", self.device.queue_submit(self.queue, &[vk::SubmitInfo::default().command_buffers(&cbs)], self.fence))?;
            ve("wait", self.device.wait_for_fences(&[self.fence], true, u64::MAX))?;
            ve("reset", self.device.reset_fences(&[self.fence]))
        }
    }

    /// The outline of `gray` (`w`×`h` as given to `new`): 0 on an outline
    /// pixel (or a dark one), 255 elsewhere, as `edges::outline` makes it.
    pub fn outline(&mut self, gray: &[u8]) -> Result<Vec<u8>, String> {
        let px = self.size.0 * self.size.1;
        if gray.len() != px {
            return Err("frame size changed".into());
        }
        unsafe { std::ptr::copy_nonoverlapping(gray.as_ptr(), self.buffers[INPUT].ptr, px) };
        self.submit(0)?;
        let changed = || unsafe { (self.buffers[CHANGED].ptr as *const u32).read_volatile() };
        let mut batches = 1;
        while changed() != 0 && batches < MAX_BATCHES {
            self.submit(1)?;
            batches += 1;
        }
        self.submit(2)?;
        let mut out = vec![0u8; px];
        unsafe { std::ptr::copy_nonoverlapping(self.buffers[RESULT].ptr, out.as_mut_ptr(), px) };
        Ok(out)
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.pool, None);
            for &p in &self.pipelines {
                self.device.destroy_pipeline(p, None);
            }
            self.device.destroy_shader_module(self.module, None);
            self.device.destroy_pipeline_layout(self.pipe_layout, None);
            self.device.destroy_descriptor_pool(self.desc_pool, None);
            self.device.destroy_descriptor_set_layout(self.set_layout, None);
            for b in &self.buffers {
                self.device.destroy_buffer(b.buffer, None);
                self.device.free_memory(b.memory, None);
            }
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The GPU outline is the CPU one, pixel for pixel. Skipped where there
    /// is no GPU (a CI runner, say).
    #[test]
    fn matches_the_cpu() {
        let (w, h) = (203, 157);
        let mut gpu = match Gpu::new(w, h, crate::edges::DARK as u32) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipped, no GPU: {e}");
                return;
            }
        };
        // Blobs and ramps with some noise: strong and weak edges, all directions.
        let mut seed = 7u32;
        let mut rand = move || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            seed >> 8
        };
        let mut img = vec![230u8; w * h];
        for y in 0..h {
            for x in 0..60 {
                img[y * w + x] = (x * 4) as u8;
            }
        }
        for _ in 0..120 {
            let (bw, bh, x0, y0, v) = (3 + rand() as usize % 20, 3 + rand() as usize % 20, rand() as usize % (w - 24), rand() as usize % (h - 24), (rand() % 200) as u8);
            for y in y0..y0 + bh {
                for x in x0..x0 + bw {
                    img[y * w + x] = v;
                }
            }
        }
        for v in img.iter_mut() {
            *v = v.saturating_add((rand() % 5) as u8);
        }
        let mut want = crate::edges::outline(&img, w, h);
        crate::edges::threshold(&img, &mut want);
        let got = gpu.outline(&img).expect("gpu frame");
        let differing = want.iter().zip(&got).filter(|(a, b)| a != b).count();
        assert_eq!(differing, 0, "{differing} of {} pixels differ on {}", w * h, gpu.name());
        // Frames can follow one another.
        assert_eq!(gpu.outline(&img).expect("second frame"), got);
    }
}
