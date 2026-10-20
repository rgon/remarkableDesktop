//! The picture path on the GPU through Vulkan: the raw BGRx capture becomes
//! the panel's gray frame (scaling, letterbox bars, rotation), and for the
//! edge view that frame goes through blur, Sobel, suppression, hysteresis,
//! line placement and dark pixels, then is compared with the previous frame
//! (which tiles changed) and packed to bits. The CPU only copies the capture
//! in and the result out, so a busy CPU is not in the way of the picture.
//! `libvulkan` is loaded at run time; without a Vulkan GPU `Gpu::new` fails.
//!
//! One compute pipeline per stage of `edges.wgsl`, all sharing one
//! descriptor set of storage buffers. The work is recorded once into two
//! command buffers (the edge view; the gray view) and each frame is one
//! submit and one fence wait. Hysteresis is connected-component labeling
//! (union-find) in a fixed number of dispatches, so nothing is read back
//! to the CPU in between. Must be created and used on one thread.

use crate::ghost::{self, Drive, Kind};
use ash::{Device, Entry, Instance, vk};
use std::ffi::CStr;

/// The shader, as built by build.rs.
const SPIRV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/edges.spv"));

/// Entry points of the shader, in `Stage` order.
const STAGES: [&CStr; 17] = [c"blur_h", c"blur_v", c"sobel", c"nms", c"hyst", c"place_line", c"dark_px", c"diff_tiles", c"pack_bits", c"convert", c"pack_gray4", c"diff_gray4", c"cc_init", c"cc_link", c"cc_flag", c"ghost_drive", c"ghost_err"];

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
    PackGray4,
    DiffGray4,
    CcInit,
    CcLink,
    CcFlag,
    GhostDrive,
    GhostErr,
}

/// Gray (0..=255) under which a pixel is drawn black, outline or not.
pub const DARK: u32 = 100;

/// Storage buffer bindings of `edges.wgsl`.
const INPUT: usize = 0;
const OUTLINE: usize = 5;
const BITS: usize = 7;
const PARAMS: usize = 8;
const TILES: usize = 9;
const PREV: usize = 10;
const SRC: usize = 11;
const GRAY4: usize = 12;
const PREV4: usize = 13;
// The ghosting model (ghost.rs): the estimate per pixel (a word each, like
// the other per-pixel buffers), the picture an update sent, the tiles it
// covered, and the error per tile and per 8x8 cell. Without the model they
// are four bytes each.
#[allow(dead_code)] // only the shader reads it
const EST: usize = 14;
const SENT: usize = 15;
const GMASK: usize = 16;
const GERR: usize = 17;
const GCELL: usize = 18;
const BUFFERS: usize = 19;

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
    /// The capture's bytes are R, G, B, x (not B, G, R, x).
    pub swap_rb: bool,
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
    /// The edge view, the gray view; with the ghosting model also the
    /// drive of an update that was sent, and the error against the newest frame.
    cmds: Vec<vk::CommandBuffer>,
    /// The ghosting model's parameters (None: no model).
    ghost: Option<ghost::Params>,
    /// The kind of the newest frame (what the error is measured against).
    desired: Option<Kind>,
    /// Cells of 8x8 pixels in the error map.
    #[allow(dead_code)]
    cells: usize,
    fence: vk::Fence,
    size: (usize, usize),
    /// Tiles in the grid of `tiles.rs`.
    tiles: usize,
    /// Bytes of a one-word-per-pixel buffer.
    words: u64,
    /// A frame has been through each view, so `PREV` / `PREV4` hold something to compare with.
    have_prev: bool,
    have_prev4: bool,
    /// Bytes of the 4-bit frame buffers.
    gray4_bytes: u64,
    dark: u32,
    convert: Convert,
    name: String,
}

/// The gray view of one frame.
pub struct Gray4 {
    /// The frame as `gray4` packs it: 4-bit levels, two pixels a byte, rows padded to bytes.
    pub bytes: Vec<u8>,
    /// The tiles of the grid that differ from the previous frame (None for the first).
    pub dirty: Option<Vec<bool>>,
    /// The ghosting model's error per tile against this frame (None without the model).
    pub ghost: Option<Vec<u32>>,
}

/// One frame's result (the edge view).
pub struct Outline {
    /// The frame as `bits1` packs it: one bit per pixel, set where the
    /// pixel is white (not on an outline and not dark), rows padded to bytes.
    pub bits: Vec<u8>,
    /// The tiles of the grid that differ from the previous frame (None for the first).
    pub dirty: Option<Vec<bool>>,
    pub ghost: Option<Vec<u32>>,
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
        Self::build(w, h, dark, convert, None)
    }

    /// As `new`, with the ghosting model (ghost.rs) on: the panel is assumed
    /// white to begin with.
    pub fn with_ghost(w: usize, h: usize, dark: u32, convert: Convert, params: ghost::Params) -> Result<Gpu, String> {
        let mut gpu = Self::build(w, h, dark, convert, Some(params))?;
        let all = vec![true; gpu.tiles];
        gpu.ghost_drive(None, Kind::Gray4, Drive::Reset, &all)?;
        Ok(gpu)
    }

    fn build(w: usize, h: usize, dark: u32, convert: Convert, ghost: Option<ghost::Params>) -> Result<Gpu, String> {
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
        let gray4_bytes = ((w.div_ceil(2) * h) as u64).div_ceil(4).max(1) * 4;
        let cells = w.div_ceil(ghost::CELL) * h.div_ceil(ghost::CELL);
        let on = ghost.is_some();
        let small = |bytes: u64| if on { bytes.max(4) } else { 4 };
        // (size, host access) per binding.
        let layout = [
            (packed, true),
            (words, false),
            (words, false),
            (words, false),
            (words, false),
            (words, false),
            (words, false),
            (bits_bytes, true),
            (128, true),
            ((tiles as u64 * 4).max(4), true),
            (words, false),
            ((convert.src.0 * convert.src.1 * 4) as u64, true),
            (gray4_bytes, true),
            (gray4_bytes, false),
            (small(words), false),
            (small(gray4_bytes), true),
            (small(tiles as u64 * 4), true),
            (small(tiles as u64 * 4), true),
            (small(cells as u64 * 4), true),
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
        let cmds = ve("command buffers", unsafe { device.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).level(vk::CommandBufferLevel::PRIMARY).command_buffer_count(if on { 4 } else { 2 })) })?;
        let fence = ve("fence", unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) })?;

        let gpu = Gpu { _entry: entry, instance, device, queue, pool, set_layout, desc_pool, pipe_layout, module, pipelines, buffers, cmds, ghost, desired: None, cells, fence, size: (w, h), tiles, words, have_prev: false, have_prev4: false, gray4_bytes, dark, convert, name };
        gpu.write_params([0.0, 1.0, 0.0, 1.0], 0, Kind::Gray4);
        gpu.record(set)?;
        Ok(gpu)
    }

    /// What the stages need to know about the frame, for the shader's `Params`.
    /// `shown` is where the capture sits in the tablet's logical frame
    /// (x0, x1, y0, y1 as fractions): the rest is letterbox, drawn white.
    /// `mode` and `kind` are the ghosting model's pass and picture kind.
    fn write_params(&self, shown: [f64; 4], mode: u32, kind: Kind) {
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
        let g = self.ghost.unwrap_or_default();
        let words: [u32; 22] = [
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
            mode,
            kind as u32,
            g.k_paint,
            g.k_erase,
            g.k_paint_edge,
            g.k_erase_edge,
            g.k_settle,
            g.threshold,
            g.dead_band,
            c.swap_rb as u32,
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
        let clear_err = |cb: vk::CommandBuffer| unsafe {
            d.cmd_fill_buffer(cb, self.buffers[GERR].buffer, 0, vk::WHOLE_SIZE, 0);
            d.cmd_fill_buffer(cb, self.buffers[GCELL].buffer, 0, vk::WHOLE_SIZE, 0);
        };
        let run = |cb: vk::CommandBuffer, stage: Stage| unsafe {
            d.cmd_bind_pipeline(cb, vk::PipelineBindPoint::COMPUTE, self.pipelines[stage as usize]);
            d.cmd_dispatch(cb, groups.0, groups.1, 1);
            sync(cb);
        };
        for (i, &cb) in self.cmds.iter().enumerate() {
            unsafe {
                ve("begin", d.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default()))?;
                d.cmd_bind_descriptor_sets(cb, vk::PipelineBindPoint::COMPUTE, self.pipe_layout, 0, &[set], &[]);
                match i {
                    0 => {
                        d.cmd_fill_buffer(cb, self.buffers[OUTLINE].buffer, 0, vk::WHOLE_SIZE, 0xffff_ffff);
                        d.cmd_fill_buffer(cb, self.buffers[TILES].buffer, 0, vk::WHOLE_SIZE, 0);
                        if self.ghost.is_some() {
                            clear_err(cb);
                        }
                        sync(cb);
                        for stage in [Stage::Convert, Stage::BlurH, Stage::BlurV, Stage::Sobel, Stage::Nms, Stage::CcInit, Stage::CcLink, Stage::CcFlag, Stage::Hyst, Stage::Place, Stage::Dark, Stage::Diff] {
                            run(cb, stage);
                        }
                        // This frame is the next one's reference.
                        let copy = [vk::BufferCopy::default().size(self.words)];
                        d.cmd_copy_buffer(cb, self.buffers[OUTLINE].buffer, self.buffers[PREV].buffer, &copy);
                        sync(cb);
                        run(cb, Stage::PackBits);
                        if self.ghost.is_some() {
                            run(cb, Stage::GhostErr);
                        }
                    }
                    1 => {
                        d.cmd_fill_buffer(cb, self.buffers[TILES].buffer, 0, vk::WHOLE_SIZE, 0);
                        if self.ghost.is_some() {
                            clear_err(cb);
                        }
                        sync(cb);
                        run(cb, Stage::Convert);
                        run(cb, Stage::PackGray4);
                        run(cb, Stage::DiffGray4);
                        // This frame is the next one's reference.
                        let copy = [vk::BufferCopy::default().size(self.gray4_bytes)];
                        d.cmd_copy_buffer(cb, self.buffers[GRAY4].buffer, self.buffers[PREV4].buffer, &copy);
                        sync(cb);
                        if self.ghost.is_some() {
                            run(cb, Stage::GhostErr);
                        }
                    }
                    // An update that was sent, applied to the estimate.
                    2 => run(cb, Stage::GhostDrive),
                    // The error of the estimate against the newest frame, between frames.
                    _ => {
                        clear_err(cb);
                        sync(cb);
                        run(cb, Stage::GhostErr);
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
        self.upload(raw, shown, Kind::Bits)?;
        self.frame()
    }

    /// The gray view of a raw BGRx capture: the panel frame as 4-bit levels
    /// (packed as `gray4` does), and the tiles that changed since the
    /// previous call.
    pub fn gray_bgrx(&mut self, raw: &[u8], shown: [f64; 4]) -> Result<Gray4, String> {
        self.upload(raw, shown, Kind::Gray4)?;
        self.submit(1)?;
        let mut bytes = vec![0u8; self.size.0.div_ceil(2) * self.size.1];
        unsafe { std::ptr::copy_nonoverlapping(self.buffers[GRAY4].ptr, bytes.as_mut_ptr(), bytes.len()) };
        let flags = unsafe { std::slice::from_raw_parts(self.buffers[TILES].ptr as *const u32, self.tiles) };
        let dirty = self.have_prev4.then(|| flags.iter().map(|&f| f != 0).collect());
        self.have_prev4 = true;
        Ok(Gray4 { bytes, dirty, ghost: self.read_ghost() })
    }

    /// The one copy of a frame on its way to the GPU: from `raw` (rows of
    /// `raw.len() / height` bytes, at least a row of pixels each) into the
    /// source buffer, which is the host-visible memory the shader reads.
    fn upload(&mut self, raw: &[u8], shown: [f64; 4], kind: Kind) -> Result<(), String> {
        let c = self.convert;
        let (w, h) = c.src;
        let row = w * 4;
        if raw.len() < row * h || raw.len() % h != 0 {
            return Err("capture size changed".into());
        }
        let stride = raw.len() / h;
        self.write_params(shown, 0, kind);
        self.desired = Some(kind);
        let dst = self.buffers[SRC].ptr;
        unsafe {
            if stride == row {
                std::ptr::copy_nonoverlapping(raw.as_ptr(), dst, row * h);
            } else {
                for y in 0..h {
                    std::ptr::copy_nonoverlapping(raw.as_ptr().add(y * stride), dst.add(y * row), row);
                }
            }
        }
        Ok(())
    }

    /// Runs the stages on the capture in the source buffer.
    fn frame(&mut self) -> Result<Outline, String> {
        self.submit(0)?;
        let mut bits = vec![0u8; self.size.0.div_ceil(8) * self.size.1];
        unsafe { std::ptr::copy_nonoverlapping(self.buffers[BITS].ptr, bits.as_mut_ptr(), bits.len()) };
        let flags = unsafe { std::slice::from_raw_parts(self.buffers[TILES].ptr as *const u32, self.tiles) };
        let dirty = self.have_prev.then(|| flags.iter().map(|&f| f != 0).collect());
        self.have_prev = true;
        Ok(Outline { bits, dirty, ghost: self.read_ghost() })
    }

    /// The model's error per tile after the last pass that made it.
    fn read_ghost(&self) -> Option<Vec<u32>> {
        self.ghost?;
        Some(unsafe { std::slice::from_raw_parts(self.buffers[GERR].ptr as *const u32, self.tiles) }.to_vec())
    }

    /// The model's error per 8x8 cell (`ghost::CELL`), row by row, after the
    /// last pass that made it: the map a step that boxes ghosts starts from.
    /// It stays in `GCELL` on the GPU for such a step to read.
    #[allow(dead_code)] // for the step that boxes ghosts
    pub fn ghost_cells(&self) -> Option<Vec<u32>> {
        self.ghost?;
        Some(unsafe { std::slice::from_raw_parts(self.buffers[GCELL].ptr as *const u32, self.cells) }.to_vec())
    }

    /// An update that was sent: its pixels in the marked tiles are driven as
    /// `mode` says. `picture` is what was sent (packed as `kind`), or None
    /// for the one given last (and for `Reset`).
    pub fn ghost_drive(&mut self, picture: Option<&[u8]>, kind: Kind, mode: Drive, marked: &[bool]) -> Result<(), String> {
        if self.ghost.is_none() {
            return Err("no ghosting model".into());
        }
        if let Some(p) = picture {
            let want = match kind {
                Kind::Gray4 => self.size.0.div_ceil(2) * self.size.1,
                Kind::Bits => self.size.0.div_ceil(8) * self.size.1,
            };
            if p.len() != want {
                return Err("the picture sent has the wrong size".into());
            }
            unsafe { std::ptr::copy_nonoverlapping(p.as_ptr(), self.buffers[SENT].ptr, p.len()) };
        }
        let mask = unsafe { std::slice::from_raw_parts_mut(self.buffers[GMASK].ptr as *mut u32, self.tiles) };
        mask.iter_mut().zip(marked).for_each(|(m, &b)| *m = b as u32);
        self.write_params([0.0, 1.0, 0.0, 1.0], mode as u32, kind);
        self.submit(2)
    }

    /// The error against the newest frame again, after drives and no new
    /// frame (None until one has been rendered).
    pub fn ghost_tick(&mut self) -> Result<Option<Vec<u32>>, String> {
        let Some(kind) = self.desired.filter(|_| self.ghost.is_some()) else { return Ok(None) };
        self.write_params([0.0, 1.0, 0.0, 1.0], 0, kind);
        self.submit(3)?;
        Ok(self.read_ghost())
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
    use crate::ghost::{Drive, Kind};

    const FULL: [f64; 4] = [0.0, 1.0, 0.0, 1.0];

    /// A GPU taking frames as they are (no scaling, no rotation), or None
    /// where there is no GPU (a CI runner, say): the tests then skip.
    fn gpu(w: usize, h: usize) -> Option<Gpu> {
        match Gpu::new(w, h, DARK, Convert { src: (w, h), logical: (w, h), rotation: 0, swap_rb: false }) {
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

    /// The levels of a packed 4-bit frame, one byte each.
    fn nibbles(packed: &[u8], w: usize, h: usize) -> Vec<u8> {
        let rb = w.div_ceil(2);
        (0..w * h).map(|i| { let b = packed[(i / w) * rb + (i % w) / 2]; if (i % w) % 2 == 0 { b >> 4 } else { b & 15 } }).collect()
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
        assert_eq!(next.dirty.expect("flags"), grid.dirty_packed(&again.bits, &next.bits, 8));
    }

    #[test]
    fn gray_frames_are_four_bit_levels_and_report_changed_tiles() {
        let (w, h) = (203, 157);
        let Some(mut gpu) = gpu(w, h) else { return };
        let img: Vec<u8> = (0..w * h).map(|i| ((i % w) * 255 / w) as u8).collect();
        let first = gpu.gray_bgrx(&bgrx(&img), FULL).expect("first frame");
        assert!(first.dirty.is_none());
        let levels = nibbles(&first.bytes, w, h);
        let want: Vec<u8> = img.iter().map(|&v| ((v as u16 * 15 + 127) / 255) as u8).collect();
        assert_eq!(levels, want, "the same levels gray4::level gives");
        assert_eq!(first.bytes.len(), w.div_ceil(2) * h);
        let same = gpu.gray_bgrx(&bgrx(&img), FULL).expect("second frame");
        assert!(same.dirty.expect("flags").iter().all(|&d| !d));
        // One block changes by a few levels: only its tiles are flagged.
        let mut other = img.clone();
        for y in 40..70 {
            for x in 90..140 {
                other[y * w + x] = other[y * w + x].saturating_add(60);
            }
        }
        let next = gpu.gray_bgrx(&bgrx(&other), FULL).expect("third frame");
        let grid = crate::tiles::Grid::new(w, h);
        assert_eq!(next.dirty.expect("flags"), grid.dirty_packed(&same.bytes, &next.bytes, 2));
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
        let mut gpu = match Gpu::new(panel.0, panel.1, 0, Convert { src: src_size, logical, rotation: 90, swap_rb: false }) {
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
        let got = nibbles(&gpu.gray_bgrx(&raw, shown).expect("gpu frame").bytes, panel.0, panel.1);
        assert_eq!(got.len(), want.len());
        // Within a level (the gray itself may be a step off, which can flip a level).
        let level = |v: u8| ((v as u16 * 15 + 127) / 255) as i32;
        let worst = want.iter().zip(&got).map(|(a, b)| (level(*a) - *b as i32).abs()).max().unwrap();
        assert!(worst <= 1, "differs by {worst} levels somewhere on {}", gpu.name());
        // The bars are white: the panel's first and last columns are the logical frame's top and bottom rows.
        assert!(got.chunks(panel.0).all(|row| row[0] == 15 && row[panel.0 - 1] == 15));
    }

    /// A pseudo-random image of blocks, as 8-bit grays that are exactly a
    /// 4-bit level each (17 a level), and the levels.
    fn blocks(w: usize, h: usize, seed: u32) -> (Vec<u8>, Vec<u8>) {
        let mut s = seed;
        let mut rand = move || {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            s >> 8
        };
        let mut levels = vec![15u8; w * h];
        for _ in 0..40 {
            let (bw, bh, x0, y0, v) = (1 + rand() as usize % 20, 1 + rand() as usize % 20, rand() as usize % (w - 1), rand() as usize % (h - 1), (rand() % 16) as u8);
            for y in y0..(y0 + bh).min(h) {
                for x in x0..(x0 + bw).min(w) {
                    levels[y * w + x] = v;
                }
            }
        }
        (levels.iter().map(|&l| l * 17).collect(), levels)
    }

    /// The model on the GPU against the one on the CPU: the same drives, the
    /// same errors per tile and per cell, to the unit.
    #[test]
    fn the_ghost_model_matches_the_cpu_reference_in_the_gray_view() {
        let (w, h) = (70, 45);
        let convert = Convert { src: (w, h), logical: (w, h), rotation: 0, swap_rb: false };
        let Ok(mut gpu) = Gpu::with_ghost(w, h, DARK, convert, ghost::Params::default()) else {
            eprintln!("skipped, no GPU");
            return;
        };
        let grid = crate::tiles::Grid::new(w, h);
        let (cols, rows) = (w.div_ceil(crate::tiles::TILE), h.div_ceil(crate::tiles::TILE));
        let mut cpu = ghost::Model::new(w, h, ghost::Params::default(), Kind::Gray4);
        let mask = |m: usize| -> Vec<bool> { (0..grid.tiles()).map(|t| t % m != 1).collect() };
        let (a_gray, a) = blocks(w, h, 3);
        let (b_gray, b) = blocks(w, h, 4);
        let ga = gpu.gray_bgrx(&bgrx(&a_gray), FULL).expect("frame a");
        assert_eq!(nibbles(&ga.bytes, w, h), a);
        assert_eq!(ga.ghost.expect("errors"), cpu.errors(&a, cols, rows), "against the assumed white panel");
        // A was sent everywhere, fast; B is then rendered and measured against what the panel got.
        let all = vec![true; grid.tiles()];
        gpu.ghost_drive(Some(&ga.bytes), Kind::Gray4, Drive::Fast, &all).unwrap();
        cpu.drive(&a, Drive::Fast, &all, cols);
        let gb = gpu.gray_bgrx(&bgrx(&b_gray), FULL).expect("frame b");
        assert_eq!(gb.ghost.expect("errors"), cpu.errors(&b, cols, rows));
        // B goes out in some tiles fast, then a settle in others; measured between frames.
        gpu.ghost_drive(Some(&gb.bytes), Kind::Gray4, Drive::Fast, &mask(2)).unwrap();
        cpu.drive(&b, Drive::Fast, &mask(2), cols);
        gpu.ghost_drive(None, Kind::Gray4, Drive::Settle, &mask(3)).unwrap();
        cpu.drive(&b, Drive::Settle, &mask(3), cols);
        assert_eq!(gpu.ghost_tick().unwrap().expect("errors"), cpu.errors(&b, cols, rows));
        assert_eq!(gpu.ghost_cells().expect("cells"), cpu.cell_errors(&b));
        // A reset forgets it all.
        gpu.ghost_drive(None, Kind::Gray4, Drive::Reset, &all).unwrap();
        cpu.drive(&b, Drive::Reset, &all, cols);
        assert_eq!(gpu.ghost_tick().unwrap().expect("errors"), cpu.errors(&b, cols, rows));
    }

    /// The same in the edge view, whose frames are bits: a bar moved leaves
    /// a trail in the estimate, and a redrive takes it down.
    #[test]
    fn the_ghost_model_matches_the_cpu_reference_in_the_edge_view() {
        let (w, h) = (70, 45);
        let convert = Convert { src: (w, h), logical: (w, h), rotation: 0, swap_rb: false };
        let Ok(mut gpu) = Gpu::with_ghost(w, h, DARK, convert, ghost::Params::default()) else {
            eprintln!("skipped, no GPU");
            return;
        };
        let grid = crate::tiles::Grid::new(w, h);
        let (cols, rows) = (w.div_ceil(crate::tiles::TILE), h.div_ceil(crate::tiles::TILE));
        let mut cpu = ghost::Model::new(w, h, ghost::Params::default(), Kind::Bits);
        let all = vec![true; grid.tiles()];
        // A dark bar, moved a column at a time: its outline moves with it.
        let bar = |x0: usize| -> Vec<u8> { (0..w * h).map(|i| if (x0..x0 + 20).contains(&(i % w)) && (10..30).contains(&(i / w)) { 20 } else { 255 }).collect() };
        let levels = |o: &Outline| -> Vec<u8> { unpack(&o.bits, w, h).iter().map(|&v| if v == 255 { 15 } else { 0 }).collect() };
        let mut last = vec![];
        for x0 in [10, 11, 12] {
            let o = gpu.outline_bgrx(&bgrx(&bar(x0)), FULL).expect("frame");
            let l = levels(&o);
            assert_eq!(o.ghost.expect("errors"), cpu.errors(&l, cols, rows));
            gpu.ghost_drive(Some(&o.bits), Kind::Bits, Drive::Fast, &all).unwrap();
            cpu.drive(&l, Drive::Fast, &all, cols);
            last = l;
        }
        let errs = gpu.ghost_tick().unwrap().expect("errors");
        assert_eq!(errs, cpu.errors(&last, cols, rows));
        assert!(errs.iter().any(|&e| e > 0), "a moved outline leaves residue");
        assert_eq!(gpu.ghost_cells().expect("cells"), cpu.cell_errors(&last));
        gpu.ghost_drive(None, Kind::Bits, Drive::Redrive, &all).unwrap();
        cpu.drive(&last, Drive::Redrive, &all, cols);
        let after = gpu.ghost_tick().unwrap().expect("errors");
        assert_eq!(after, cpu.errors(&last, cols, rows));
        assert!(after.iter().sum::<u32>() < errs.iter().sum::<u32>());
    }

    /// Text-like busy frame with long edges that fade from strong to weak.
    fn busy(w: usize, h: usize) -> Vec<u8> {
        let mut seed = 11u32;
        let mut rand = move || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            seed >> 8
        };
        let mut img = vec![245u8; w * h];
        // Glyph-like strokes: short bars and boxes.
        for _ in 0..(w * h / 400) {
            let (bw, bh, x0, y0, v) = (1 + rand() as usize % 9, 1 + rand() as usize % 9, rand() as usize % (w - 10), rand() as usize % (h - 10), (rand() % 140) as u8);
            for y in y0..y0 + bh {
                for x in x0..x0 + bw {
                    img[y * w + x] = v;
                }
            }
        }
        // Long rules whose contrast fades along their length.
        for k in 0..40 {
            let y0 = 20 + k * (h - 40) / 40;
            for x in 0..w {
                let c = 110 - (x * 80 / w) as i32;
                img[y0 * w + x] = (245 - c) as u8;
                img[(y0 + 1) * w + x] = (245 - c) as u8;
            }
        }
        for k in 0..30 {
            let x0 = 20 + k * (w - 40) / 30;
            for y in 0..h {
                let c = 30 + (y * 80 / h) as i32;
                img[y * w + x0] = (245 - c) as u8;
            }
        }
        img
    }

    /// Time of `outline_bgrx` on a busy full-size frame (`cargo test --release -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn times_the_edge_view_on_a_busy_frame() {
        let (w, h) = (1872, 1404);
        let Some(mut gpu) = gpu(w, h) else { return };
        let raw = bgrx(&busy(w, h));
        let first = gpu.outline_bgrx(&raw, FULL).expect("frame");
        let n = 30;
        let t = std::time::Instant::now();
        for _ in 0..n {
            gpu.outline_bgrx(&raw, FULL).expect("frame");
        }
        let per = t.elapsed() / n;
        let black = first.bits.iter().map(|b| 8 - b.count_ones() as usize).sum::<usize>();
        let sum = first.bits.iter().fold(0u64, |a, &b| a.wrapping_mul(131).wrapping_add(b as u64));
        eprintln!("outline_bgrx: {per:?} per frame on {}, {black} black bits, checksum {sum}", gpu.name());
        let t = std::time::Instant::now();
        for _ in 0..n {
            gpu.gray_bgrx(&raw, FULL).expect("frame");
        }
        eprintln!("gray_bgrx: {:?} per frame", t.elapsed() / n);
    }

    /// A capture whose rows are padded and whose bytes come R first gives
    /// the frame of the same picture in tight BGRx.
    #[test]
    fn takes_padded_rows_and_rgb_order() {
        let (w, h) = (40usize, 24usize);
        let pixel = |x: usize, y: usize| [(x * 6) as u8, (y * 10) as u8, ((x * y) % 256) as u8];
        let tight: Vec<u8> = (0..w * h).flat_map(|i| { let [r, g, b] = pixel(i % w, i / w); [b, g, r, 0] }).collect();
        let pad = 16;
        let mut padded = vec![0xAAu8; (w * 4 + pad) * h];
        for y in 0..h {
            for x in 0..w {
                let [r, g, b] = pixel(x, y);
                padded[y * (w * 4 + pad) + x * 4..][..4].copy_from_slice(&[r, g, b, 0]);
            }
        }
        let Some(mut plain) = gpu(w, h) else { return };
        let Ok(mut other) = Gpu::new(w, h, DARK, Convert { src: (w, h), logical: (w, h), rotation: 0, swap_rb: true }) else { return };
        let want = plain.gray_bgrx(&tight, FULL).expect("gpu frame").bytes;
        assert_eq!(other.gray_bgrx(&padded, FULL).expect("gpu frame").bytes, want);
    }
}
