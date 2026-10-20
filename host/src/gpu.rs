//! The picture path on the GPU through Vulkan: the raw BGRx capture becomes
//! the panel's gray frame (scaling, letterbox bars, rotation), and for the
//! edge view that frame goes through blur, Sobel, suppression, hysteresis,
//! line placement and dark pixels, then is compared with the previous frame
//! (which tiles changed) and packed to bits. The CPU only copies the capture
//! in and the result out, so a busy CPU is not in the way of the picture.
//! `libvulkan` is loaded at run time; without a Vulkan GPU `Gpu::new` fails.
//!
//! One compute pipeline per stage of `edges.wgsl`, all sharing one
//! descriptor set of storage buffers. The work is recorded once into four
//! command buffers (setup and the first hysteresis rounds; more hysteresis
//! rounds; placement and read-back; just the conversion) and submitted for
//! each frame. Must be created and used on one thread.

use ash::{Device, Entry, Instance, vk};
use std::ffi::CStr;

/// The shader, as built by build.rs.
const SPIRV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/edges.spv"));

/// Entry points of the shader, in `Stage` order.
const STAGES: [&CStr; 10] = [c"blur_h", c"blur_v", c"sobel", c"nms", c"hyst", c"place_line", c"dark_px", c"diff_tiles", c"pack_bits", c"convert"];

#[derive(Clone, Copy)]
enum Stage {
    BlurH,
    BlurV,
    Sobel,
    Nms,
    Hyst,
    Place,
    Dark,
    Diff,
    PackBits,
    Convert,
}

/// Gray (0..=255) under which a pixel is drawn black, outline or not.
pub const DARK: u32 = 100;

/// Hysteresis rounds per submission, and submissions at most. A round
/// extends edges by one pixel, so this covers weak edge chains of 512 pixels
/// (a longer one just stays shorter than on the CPU).
const ROUNDS_PER_BATCH: usize = 8;
const MAX_BATCHES: usize = 64;

/// Storage buffer bindings of `edges.wgsl`.
const INPUT: usize = 0;
const OUTLINE: usize = 5;
const CHANGED: usize = 6;
const BITS: usize = 7;
const PARAMS: usize = 8;
const TILES: usize = 9;
const PREV: usize = 10;
const SRC: usize = 11;
const BUFFERS: usize = 12;

struct Buf {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    /// Where the CPU sees it (null for buffers only the GPU touches).
    ptr: *mut u8,
}

/// A raw BGRx capture that the GPU converts to the panel's gray frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Convert {
    /// Size of the raw capture.
    pub src: (usize, usize),
    /// The tablet's logical (unrotated) size.
    pub logical: (usize, usize),
    /// Degrees from the logical screen to the panel: 0, 90, 180 or 270.
    pub rotation: u16,
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
    /// Setup, more hysteresis, placement, conversion only.
    cmds: [vk::CommandBuffer; 4],
    fence: vk::Fence,
    size: (usize, usize),
    /// Tiles in the grid of `tiles.rs`.
    tiles: usize,
    /// Bytes of a one-word-per-pixel buffer.
    words: u64,
    /// A frame has been through, so `PREV` holds something to compare with.
    have_prev: bool,
    dark: u32,
    convert: Convert,
    name: String,
}

/// One frame's result.
pub struct Outline {
    /// The frame as `bits1` packs it: one bit per pixel, set where the
    /// pixel is white (not on an outline and not dark), rows padded to bytes.
    pub bits: Vec<u8>,
    /// The tiles of the grid that differ from the previous frame (None for the first).
    pub dirty: Option<Vec<bool>>,
}

fn ve<T>(what: &str, r: Result<T, vk::Result>) -> Result<T, String> {
    r.map_err(|e| format!("vulkan {what}: {e}"))
}

fn find_memory(props: &vk::PhysicalDeviceMemoryProperties, bits: u32, want: vk::MemoryPropertyFlags) -> Option<u32> {
    (0..props.memory_type_count).find(|&i| bits & (1 << i) != 0 && props.memory_types[i as usize].property_flags.contains(want))
}

impl Gpu {
    /// Opens the best GPU and prepares for panel frames of `w`×`h` made from
    /// raw captures as `convert` says. Pixels darker than `dark` come out
    /// black in the edge view (0: none do).
    pub fn new(w: usize, h: usize, dark: u32, convert: Convert) -> Result<Gpu, String> {
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
        let tiles = crate::tiles::Grid::new(w, h).tiles();
        let packed = px.div_ceil(4).max(1) * 4;
        let words = (px * 4).max(4);
        let bits_bytes = ((w.div_ceil(8) * h) as u64).div_ceil(4).max(1) * 4;
        // (size, host access) per binding.
        let layout = [
            (packed, true),
            (words, false),
            (words, false),
            (words, false),
            (words, false),
            (words, false),
            (4, true),
            (bits_bytes, true),
            (64, true),
            ((tiles as u64 * 4).max(4), true),
            (words, false),
            ((convert.src.0 * convert.src.1 * 4) as u64, true),
        ];
        let mut buffers = Vec::with_capacity(BUFFERS);
        for (size, host) in layout {
            buffers.push(Self::buffer(&device, &mem_props, size, host)?);
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
        let cmds = ve("command buffers", unsafe { device.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).level(vk::CommandBufferLevel::PRIMARY).command_buffer_count(4)) })?;
        let fence = ve("fence", unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) })?;

        let gpu = Gpu { _entry: entry, instance, device, queue, pool, set_layout, desc_pool, pipe_layout, module, pipelines, buffers, cmds: [cmds[0], cmds[1], cmds[2], cmds[3]], fence, size: (w, h), tiles, words, have_prev: false, dark, convert, name };
        gpu.write_params([0.0, 1.0, 0.0, 1.0]);
        gpu.record(set)?;
        Ok(gpu)
    }

    /// What the stages need to know about the frame, for the shader's `Params`.
    /// `shown` is where the capture sits in the tablet's logical frame
    /// (x0, x1, y0, y1 as fractions): the rest is letterbox, drawn white.
    fn write_params(&self, shown: [f64; 4]) {
        let (w, h) = self.size;
        let c = self.convert;
        let (lw, lh) = c.logical;
        let at = |f: f64, of: usize| (f * of as f64).round().max(0.0) as u32;
        let len = |a: f64, b: f64, of: usize| (((b - a) * of as f64).round().max(1.0)) as u32;
        let rot = match c.rotation % 360 {
            90 => 1,
            180 => 2,
            270 => 3,
            _ => 0,
        };
        let words: [u32; 12] = [
            w as u32,
            h as u32,
            self.dark,
            c.src.0 as u32,
            c.src.1 as u32,
            lw as u32,
            lh as u32,
            rot,
            at(shown[0], lw),
            at(shown[2], lh),
            len(shown[0], shown[1], lw),
            len(shown[2], shown[3], lh),
        ];
        unsafe { std::ptr::copy_nonoverlapping(words.as_ptr(), self.buffers[PARAMS].ptr as *mut u32, words.len()) };
    }

    /// The device's name, for the log.
    pub fn name(&self) -> &str {
        &self.name
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
        let info = vk::BufferCreateInfo::default().size(size).usage(vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST | vk::BufferUsageFlags::TRANSFER_SRC).sharing_mode(vk::SharingMode::EXCLUSIVE);
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

    /// Records the command buffers.
    fn record(&self, set: vk::DescriptorSet) -> Result<(), String> {
        let d = &self.device;
        let groups = (self.size.0.div_ceil(16) as u32, self.size.1.div_ceil(16) as u32);
        let shader = vk::PipelineStageFlags::COMPUTE_SHADER;
        let transfer = vk::PipelineStageFlags::TRANSFER;
        // Everything a stage wrote is there for the next one (and the fills).
        let sync = |cb: vk::CommandBuffer| unsafe {
            let b = vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::TRANSFER_READ | vk::AccessFlags::TRANSFER_WRITE);
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
                        for stage in [Stage::Convert, Stage::BlurH, Stage::BlurV, Stage::Sobel, Stage::Nms] {
                            run(cb, stage);
                        }
                        rounds(cb);
                    }
                    1 => rounds(cb),
                    3 => run(cb, Stage::Convert),
                    _ => {
                        d.cmd_fill_buffer(cb, self.buffers[TILES].buffer, 0, vk::WHOLE_SIZE, 0);
                        sync(cb);
                        run(cb, Stage::Place);
                        run(cb, Stage::Dark);
                        run(cb, Stage::Diff);
                        // This frame is the next one's reference.
                        let copy = [vk::BufferCopy::default().size(self.words)];
                        d.cmd_copy_buffer(cb, self.buffers[OUTLINE].buffer, self.buffers[PREV].buffer, &copy);
                        sync(cb);
                        run(cb, Stage::PackBits);
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

    /// The outline of a raw BGRx capture (`Convert::src` in size), `shown`
    /// being where it sits in the tablet's logical frame: the edge view of
    /// the panel frame packed to bits (set = white), and the tiles that
    /// changed since the previous call.
    pub fn outline_bgrx(&mut self, raw: &[u8], shown: [f64; 4]) -> Result<Outline, String> {
        self.upload(raw, shown)?;
        self.frame()
    }

    /// Just the gray panel frame of a raw BGRx capture, for the view that
    /// shows grays: `w * h` bytes.
    pub fn gray_bgrx(&mut self, raw: &[u8], shown: [f64; 4]) -> Result<Vec<u8>, String> {
        self.upload(raw, shown)?;
        self.submit(3)?;
        let px = self.size.0 * self.size.1;
        let mut gray = vec![0u8; px];
        unsafe { std::ptr::copy_nonoverlapping(self.buffers[INPUT].ptr, gray.as_mut_ptr(), px) };
        Ok(gray)
    }

    fn upload(&mut self, raw: &[u8], shown: [f64; 4]) -> Result<(), String> {
        let c = self.convert;
        if raw.len() != c.src.0 * c.src.1 * 4 {
            return Err("capture size changed".into());
        }
        self.write_params(shown);
        unsafe { std::ptr::copy_nonoverlapping(raw.as_ptr(), self.buffers[SRC].ptr, raw.len()) };
        Ok(())
    }

    /// Runs the stages on the capture in the source buffer.
    fn frame(&mut self) -> Result<Outline, String> {
        self.submit(0)?;
        let changed = || unsafe { (self.buffers[CHANGED].ptr as *const u32).read_volatile() };
        let mut batches = 1;
        while changed() != 0 && batches < MAX_BATCHES {
            self.submit(1)?;
            batches += 1;
        }
        self.submit(2)?;
        let mut bits = vec![0u8; self.size.0.div_ceil(8) * self.size.1];
        unsafe { std::ptr::copy_nonoverlapping(self.buffers[BITS].ptr, bits.as_mut_ptr(), bits.len()) };
        let flags = unsafe { std::slice::from_raw_parts(self.buffers[TILES].ptr as *const u32, self.tiles) };
        let dirty = self.have_prev.then(|| flags.iter().map(|&f| f != 0).collect());
        self.have_prev = true;
        Ok(Outline { bits, dirty })
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

    const FULL: [f64; 4] = [0.0, 1.0, 0.0, 1.0];

    /// A GPU taking frames as they are (no scaling, no rotation), or None
    /// where there is no GPU (a CI runner, say): the tests then skip.
    fn gpu(w: usize, h: usize) -> Option<Gpu> {
        match Gpu::new(w, h, DARK, Convert { src: (w, h), logical: (w, h), rotation: 0 }) {
            Ok(g) => Some(g),
            Err(e) => {
                eprintln!("skipped, no GPU: {e}");
                None
            }
        }
    }

    /// A gray image as the raw capture it stands for.
    fn bgrx(gray: &[u8]) -> Vec<u8> {
        gray.iter().flat_map(|&g| [g, g, g, 0]).collect()
    }

    /// The bits of a frame as one byte a pixel: 255 white, 0 black.
    fn unpack(bits: &[u8], w: usize, h: usize) -> Vec<u8> {
        let rb = w.div_ceil(8);
        (0..w * h).map(|i| if bits[(i / w) * rb + (i % w) / 8] >> (7 - (i % w) % 8) & 1 != 0 { 255 } else { 0 }).collect()
    }

    fn edges_of(gpu: &mut Gpu, gray: &[u8], w: usize, h: usize) -> Vec<u8> {
        unpack(&gpu.outline_bgrx(&bgrx(gray), FULL).expect("gpu frame").bits, w, h)
    }

    fn black_columns(out: &[u8], w: usize, row: usize) -> Vec<usize> {
        (0..w).filter(|&x| out[row * w + x] == 0).collect()
    }

    #[test]
    fn a_flat_image_has_no_edges() {
        let Some(mut gpu) = gpu(32, 32) else { return };
        assert!(edges_of(&mut gpu, &[128u8; 32 * 32], 32, 32).iter().all(|&v| v == 255));
    }

    #[test]
    fn a_step_gives_a_thin_line_on_its_dark_side() {
        let (w, h) = (32, 32);
        let Some(mut gpu) = gpu(w, h) else { return };
        // Left half gray, right half white: the line stays in the gray half.
        let img: Vec<u8> = (0..w * h).map(|i| if i % w < 16 { 120 } else { 255 }).collect();
        let black = black_columns(&edges_of(&mut gpu, &img, w, h), w, 10);
        assert!(black.len() == 1 && (14..=15).contains(&black[0]), "{black:?}");
        // And mirrored: right half gray, line in the right half.
        let img: Vec<u8> = (0..w * h).map(|i| if i % w < 16 { 255 } else { 120 }).collect();
        let black = black_columns(&edges_of(&mut gpu, &img, w, h), w, 10);
        assert!(black.len() == 1 && (16..=17).contains(&black[0]), "{black:?}");
    }

    #[test]
    fn a_dark_square_is_filled_and_a_light_one_is_not() {
        let (w, h) = (48, 32);
        let Some(mut gpu) = gpu(w, h) else { return };
        // A black square on the left and a light gray one on the right, on white.
        let img: Vec<u8> = (0..w * h)
            .map(|i| match (i % w, i / w) {
                (6..=17, 8..=23) => 0,
                (30..=41, 8..=23) => 200,
                _ => 255,
            })
            .collect();
        let out = edges_of(&mut gpu, &img, w, h);
        assert_eq!(out[16 * w + 12], 0, "dark interior is black");
        assert_eq!(out[16 * w + 36], 255, "light interior stays white");
        assert_eq!(out[16 * w + 24], 255, "background stays white");
    }

    #[test]
    fn a_weak_isolated_edge_is_dropped() {
        let (w, h) = (32, 32);
        let Some(mut gpu) = gpu(w, h) else { return };
        // A faint step (contrast 20) never reaches the high threshold.
        let img: Vec<u8> = (0..w * h).map(|i| if i % w < 16 { 100 } else { 120 }).collect();
        assert!(edges_of(&mut gpu, &img, w, h).iter().all(|&v| v == 255));
    }

    #[test]
    fn frames_report_the_tiles_that_changed() {
        let (w, h) = (203, 157);
        let Some(mut gpu) = gpu(w, h) else { return };
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
        let first = gpu.outline_bgrx(&bgrx(&img), FULL).expect("first frame");
        assert!(first.dirty.is_none(), "nothing to compare the first frame with");
        // The same frame again: nothing changed.
        let again = gpu.outline_bgrx(&bgrx(&img), FULL).expect("second frame");
        assert_eq!(again.bits, first.bits);
        assert!(again.dirty.expect("flags").iter().all(|&d| !d));
        // A different one: the tiles flagged are the ones that differ.
        let mut other = img.clone();
        for y in 40..70 {
            for x in 90..140 {
                other[y * w + x] = 255 - other[y * w + x];
            }
        }
        let next = gpu.outline_bgrx(&bgrx(&other), FULL).expect("third frame");
        let grid = crate::tiles::Grid::new(w, h);
        assert_eq!(next.dirty.expect("flags"), grid.dirty_bits(&again.bits, &next.bits));
    }

    /// The conversion on the CPU, step by step as `edges.wgsl` does it.
    fn reference(src: &[u8], (sw, sh): (usize, usize), (lw, lh): (usize, usize), rotation: u16, (w, h): (usize, usize), sc: (i32, i32, i32, i32)) -> Vec<u8> {
        let luma = |x: i32, y: i32| {
            let p = &src[(y as usize * sw + x as usize) * 4..][..4];
            0.299 * p[2] as f32 + 0.587 * p[1] as f32 + 0.114 * p[0] as f32
        };
        let mut out = vec![0u8; w * h];
        for oy in 0..h as i32 {
            for ox in 0..w as i32 {
                let (lw, lh) = (lw as i32, lh as i32);
                let (lx, ly) = match rotation {
                    90 => (lw - 1 - oy, ox),
                    180 => (lw - 1 - ox, lh - 1 - oy),
                    270 => (oy, lh - 1 - ox),
                    _ => (ox, oy),
                };
                let (rx, ry) = (lx - sc.0, ly - sc.1);
                out[oy as usize * w + ox as usize] = if rx < 0 || ry < 0 || rx >= sc.2 || ry >= sc.3 {
                    255
                } else {
                    let fx = (rx as f32 + 0.5) * sw as f32 / sc.2 as f32 - 0.5;
                    let fy = (ry as f32 + 0.5) * sh as f32 / sc.3 as f32 - 0.5;
                    let (x0, y0) = (fx.floor() as i32, fy.floor() as i32);
                    let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
                    let (maxx, maxy) = (sw as i32 - 1, sh as i32 - 1);
                    let (xa, xb, ya, yb) = (x0.clamp(0, maxx), (x0 + 1).clamp(0, maxx), y0.clamp(0, maxy), (y0 + 1).clamp(0, maxy));
                    let mix = |a: f32, b: f32, t: f32| a + (b - a) * t;
                    let top = mix(luma(xa, ya), luma(xb, ya), tx);
                    let bottom = mix(luma(xa, yb), luma(xb, yb), tx);
                    (mix(top, bottom, ty) + 0.5).clamp(0.0, 255.0) as u8
                };
            }
        }
        out
    }

    /// A raw capture turns into the panel's gray frame: scaled into the
    /// letterbox, bars white, rotated.
    #[test]
    fn converts_a_raw_capture_like_the_reference() {
        // A 320x180 capture shown in a 240x180 logical screen (bars above and
        // below), rotated a quarter turn onto a 180x240 panel.
        let (src_size, logical, panel) = ((320usize, 180usize), (240usize, 180usize), (180usize, 240usize));
        let shown = [0.0, 1.0, 0.125, 0.875];
        let mut gpu = match Gpu::new(panel.0, panel.1, 0, Convert { src: src_size, logical, rotation: 90 }) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipped, no GPU: {e}");
                return;
            }
        };
        let mut raw = vec![0u8; src_size.0 * src_size.1 * 4];
        for y in 0..src_size.1 {
            for x in 0..src_size.0 {
                let p = &mut raw[(y * src_size.0 + x) * 4..][..4];
                p.copy_from_slice(&[(x * 255 / src_size.0) as u8, (y * 255 / src_size.1) as u8, ((x + y) * 255 / (src_size.0 + src_size.1)) as u8, 0]);
            }
        }
        // 0.125 * 180 = 22.5 rounds to 23; the picture is 0.75 * 180 = 135 high.
        let want = reference(&raw, src_size, logical, 90, panel, (0, 23, 240, 135));
        let got = gpu.gray_bgrx(&raw, shown).expect("gpu frame");
        assert_eq!(got.len(), want.len());
        let worst = want.iter().zip(&got).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
        let off = want.iter().zip(&got).filter(|(a, b)| a != b).count();
        assert!(worst <= 1, "differs by {worst} somewhere ({off} pixels off) on {}", gpu.name());
        // The bars are white: the panel's first and last columns are the logical frame's top and bottom rows.
        assert!(got.chunks(panel.0).all(|row| row[0] == 255 && row[panel.0 - 1] == 255));
    }
}
