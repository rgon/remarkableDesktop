//! The picture path on the GPU through Vulkan: the raw BGRx capture becomes
//! the panel's gray frame (scaling, letterbox bars, rotation), and for the
//! edge view that frame goes through blur, Sobel, suppression, hysteresis,
//! line placement and dark pixels, then is compared with the previous frame
//! (which cells changed, and the few boxes around them, found on the GPU)
//! and packed to bits. The CPU only copies the capture
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
use crate::tiles::{self, BOXES, CELL, Rect};
use ash::{Device, Entry, Instance, vk};
use std::ffi::CStr;

/// The shader, as built by build.rs.
const SPIRV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/edges.spv"));

/// Entry points of the shader, in `Stage` order.
const STAGES: [&CStr; 24] = [
    c"blur_h", c"blur_v", c"sobel", c"nms", c"hyst", c"place_line", c"dark_px", c"diff_tiles", c"pack_bits", c"convert", c"pack_gray4", c"diff_gray4", c"cc_init", c"cc_link", c"cc_flag", c"ghost_drive", c"ghost_err", c"rc_slot", c"rc_init", c"rc_link", c"rc_box", c"rc_split", c"rc_sub",
    c"rc_emit",
];

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
    RcSlot,
    RcInit,
    RcLink,
    RcBox,
    RcSplit,
    RcSub,
    RcEmit,
}

/// The passes that turn the map of changed cells (and of cells with ghosting
/// error) into the list of boxes, in order.
const BOX_STAGES: [Stage; 6] = [Stage::RcInit, Stage::RcLink, Stage::RcBox, Stage::RcSplit, Stage::RcSub, Stage::RcEmit];

/// Frames whose changed cells the history word of a cell keeps (a bit each).
const HISTORY: u64 = 32;
/// Words a layer takes in the list buffer: a count, a counter, two spare, the boxes.
const LIST_WORDS: usize = 4 + BOXES * 4;

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
// The rectangles: the history of changed cells, the working storage and the
// list of boxes (what the CPU reads).
#[allow(dead_code)] // only the shader reads them
const CWORD: usize = 19;
#[allow(dead_code)]
const RC: usize = 20;
const RLIST: usize = 21;
const BUFFERS: usize = 22;

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
    /// Cells of 8x8 pixels in the maps.
    cells: usize,
    /// Layers of the box passes: the changed cells, and with the model the
    /// cells with ghosting error.
    layers: u32,
    /// The frame number of the last frame, and of the first of the run of
    /// consecutive frames it ends (the history only answers inside a run).
    last_seq: Option<u64>,
    chain_from: u64,
    /// What the newest frame asked the box passes for (see `Gpu::upload`).
    ask: Ask,
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

/// Where a frame stands in the stream, for the boxes of what changed: its
/// number (a frame is one more than the one before, or the run starts anew)
/// and the number of the frame the sender has, whose picture the boxes must
/// be against (None: no boxes). Boxes are answered for frames at most 31
/// back inside a run of consecutive numbers, which all went through the same view.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frame {
    pub seq: u64,
    pub base: Option<u64>,
}

/// What the box passes were asked for with a frame: the slot of the frame
/// in the history words, the slots of the frames since the base, whether
/// nothing precedes the frame, and whether the answer is good.
#[derive(Clone, Copy, Default)]
struct Ask {
    slot: u32,
    mask: u32,
    full: bool,
    valid: bool,
}

/// The gray view of one frame.
pub struct Gray4 {
    /// The frame as `gray4` packs it: 4-bit levels, two pixels a byte, rows padded to bytes.
    pub bytes: Vec<u8>,
    /// The tiles of the grid that differ from the previous frame (None for the first).
    pub dirty: Option<Vec<bool>>,
    /// Boxes (x, y, w, h) that together cover every pixel that differs from
    /// the frame the call named as the base (see `Frame`), found on the GPU
    /// in cells of 8x8, on whole bytes of the packing: None when there is
    /// no such answer.
    pub boxes: Option<Vec<Rect>>,
    /// The ghosting model's error per tile against this frame (None without the model).
    pub ghost: Option<Vec<u32>>,
    /// Boxes around the cells whose error is notable (`ghost::CELL_ERR`).
    pub ghost_boxes: Option<Vec<Rect>>,
}

/// One frame's result (the edge view).
pub struct Outline {
    /// The frame as `bits1` packs it: one bit per pixel, set where the
    /// pixel is white (not on an outline and not dark), rows padded to bytes.
    pub bits: Vec<u8>,
    /// The tiles of the grid that differ from the previous frame (None for the first).
    pub dirty: Option<Vec<bool>>,
    pub boxes: Option<Vec<Rect>>,
    pub ghost: Option<Vec<u32>>,
    pub ghost_boxes: Option<Vec<Rect>>,
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
        let all = vec![true; gpu.cells];
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
        let cells = w.div_ceil(CELL) * h.div_ceil(CELL);
        let on = ghost.is_some();
        let layers = if on { 2 } else { 1 };
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
            (small(cells as u64 * 4), true),
            (small(tiles as u64 * 4), true),
            (small(cells as u64 * 4), true),
            // The cell history, 18 words a cell and layer of working storage, the boxes.
            ((cells as u64 * 4).max(4), false),
            ((cells as u64 * 72 * layers as u64).max(4), false),
            ((2 * LIST_WORDS * 4) as u64, true),
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
        let cmds = ve("command buffers", unsafe { device.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).level(vk::CommandBufferLevel::PRIMARY).command_buffer_count(if on { 4 } else { 2 } + cfg!(test) as u32)) })?;
        let fence = ve("fence", unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) })?;

        let gpu = Gpu { _entry: entry, instance, device, queue, pool, set_layout, desc_pool, pipe_layout, module, pipelines, buffers, cmds, ghost, desired: None, cells, layers, last_seq: None, chain_from: 0, ask: Ask::default(), fence, size: (w, h), tiles, words, have_prev: false, have_prev4: false, gray4_bytes, dark, convert, name };
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
        let words: [u32; 25] = [
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
            self.ask.slot,
            self.ask.mask,
            self.ask.full as u32,
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
        // The passes over the cells: one z per layer.
        let cell_groups = (self.size.0.div_ceil(CELL).div_ceil(16) as u32, self.size.1.div_ceil(CELL).div_ceil(16) as u32);
        let run_cells = |cb: vk::CommandBuffer, stage: Stage, layers: u32| unsafe {
            d.cmd_bind_pipeline(cb, vk::PipelineBindPoint::COMPUTE, self.pipelines[stage as usize]);
            d.cmd_dispatch(cb, cell_groups.0, cell_groups.1, layers);
            sync(cb);
        };
        let boxes = |cb: vk::CommandBuffer| BOX_STAGES.iter().for_each(|&s| run_cells(cb, s, self.layers));
        for (i, &cb) in self.cmds.iter().enumerate() {
            unsafe {
                ve("begin", d.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default()))?;
                d.cmd_bind_descriptor_sets(cb, vk::PipelineBindPoint::COMPUTE, self.pipe_layout, 0, &[set], &[]);
                match i {
                    // Tests only: the box passes alone, to time them.
                    _ if cfg!(test) && i + 1 == self.cmds.len() => boxes(cb),
                    0 => {
                        d.cmd_fill_buffer(cb, self.buffers[OUTLINE].buffer, 0, vk::WHOLE_SIZE, 0xffff_ffff);
                        d.cmd_fill_buffer(cb, self.buffers[TILES].buffer, 0, vk::WHOLE_SIZE, 0);
                        if self.ghost.is_some() {
                            clear_err(cb);
                        }
                        sync(cb);
                        run_cells(cb, Stage::RcSlot, 1);
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
                        boxes(cb);
                    }
                    1 => {
                        d.cmd_fill_buffer(cb, self.buffers[TILES].buffer, 0, vk::WHOLE_SIZE, 0);
                        if self.ghost.is_some() {
                            clear_err(cb);
                        }
                        sync(cb);
                        run_cells(cb, Stage::RcSlot, 1);
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
                        boxes(cb);
                    }
                    // An update that was sent, applied to the estimate.
                    2 => run(cb, Stage::GhostDrive),
                    // The error of the estimate against the newest frame, between frames.
                    _ => {
                        clear_err(cb);
                        sync(cb);
                        run(cb, Stage::GhostErr);
                        boxes(cb);
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
    #[cfg(test)]
    pub fn outline_bgrx(&mut self, raw: &[u8], shown: [f64; 4]) -> Result<Outline, String> {
        self.outline_frame(raw, shown, None)
    }

    /// As `outline_bgrx`, the frame being `at` in the stream (None: the one
    /// after the last, with the last as the base of the boxes).
    pub fn outline_frame(&mut self, raw: &[u8], shown: [f64; 4], at: Option<Frame>) -> Result<Outline, String> {
        self.upload(raw, shown, Kind::Bits, at)?;
        self.frame()
    }

    /// The gray view of a raw BGRx capture: the panel frame as 4-bit levels
    /// (packed as `gray4` does), and the tiles that changed since the
    /// previous call.
    #[cfg(test)]
    pub fn gray_bgrx(&mut self, raw: &[u8], shown: [f64; 4]) -> Result<Gray4, String> {
        self.gray_frame(raw, shown, None)
    }

    /// As `gray_bgrx`, the frame being `at` in the stream.
    pub fn gray_frame(&mut self, raw: &[u8], shown: [f64; 4], at: Option<Frame>) -> Result<Gray4, String> {
        self.upload(raw, shown, Kind::Gray4, at)?;
        self.submit(1)?;
        let mut bytes = vec![0u8; self.size.0.div_ceil(2) * self.size.1];
        unsafe { std::ptr::copy_nonoverlapping(self.buffers[GRAY4].ptr, bytes.as_mut_ptr(), bytes.len()) };
        let flags = unsafe { std::slice::from_raw_parts(self.buffers[TILES].ptr as *const u32, self.tiles) };
        let dirty = self.have_prev4.then(|| flags.iter().map(|&f| f != 0).collect());
        self.have_prev4 = true;
        Ok(Gray4 { bytes, dirty, boxes: self.read_changed(), ghost: self.read_ghost(), ghost_boxes: self.ghost_boxes() })
    }

    /// The one copy of a frame on its way to the GPU: from `raw` (rows of
    /// `raw.len() / height` bytes, at least a row of pixels each) into the
    /// source buffer, which is the host-visible memory the shader reads.
    /// `at` says what the boxes of changed cells are asked for.
    fn upload(&mut self, raw: &[u8], shown: [f64; 4], kind: Kind, at: Option<Frame>) -> Result<(), String> {
        let c = self.convert;
        let (w, h) = c.src;
        let row = w * 4;
        if raw.len() < row * h || raw.len() % h != 0 {
            return Err("capture size changed".into());
        }
        let stride = raw.len() / h;
        let seq = at.map_or(self.last_seq.map_or(1, |s| s + 1), |f| f.seq);
        let base = at.map_or(self.last_seq, |f| f.base);
        if self.last_seq.map(|s| s + 1) != Some(seq) {
            self.chain_from = seq;
        }
        self.last_seq = Some(seq);
        let have = if kind == Kind::Bits { self.have_prev } else { self.have_prev4 };
        let base = base.filter(|&b| have && b < seq && seq - b < HISTORY && b >= self.chain_from);
        let mask = base.map_or(0, |b| (b + 1..=seq).fold(0u32, |m, f| m | 1 << (f % HISTORY)));
        self.ask = Ask { slot: (seq % HISTORY) as u32, mask, full: !have, valid: base.is_some() };
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
        Ok(Outline { bits, dirty, boxes: self.read_changed(), ghost: self.read_ghost(), ghost_boxes: self.ghost_boxes() })
    }

    /// The boxes of one layer of the list buffer, in pixels: the frame's
    /// changed cells (0) or the cells with ghosting error (1).
    fn read_boxes(&self, layer: usize) -> Vec<Rect> {
        let list = unsafe { std::slice::from_raw_parts(self.buffers[RLIST].ptr as *const u32, 2 * LIST_WORDS) };
        let at = layer * LIST_WORDS;
        let n = (list[at] as usize).min(BOXES);
        (0..n)
            .map(|i| {
                let b = &list[at + 4 + i * 4..][..4];
                tiles::box_rect((b[0] as usize, b[1] as usize, b[2] as usize, b[3] as usize), self.size.0, self.size.1)
            })
            .collect()
    }

    /// The boxes of what changed since the frame the call named as the base.
    fn read_changed(&self) -> Option<Vec<Rect>> {
        self.ask.valid.then(|| self.read_boxes(0))
    }

    /// Boxes around the cells whose ghosting error is notable, after the
    /// last pass that made the error map (None without the model).
    pub fn ghost_boxes(&self) -> Option<Vec<Rect>> {
        self.ghost?;
        Some(self.read_boxes(1))
    }

    /// Runs the box passes alone `n` times on the maps the last frame left
    /// and returns the time per run, one submit and wait each.
    #[cfg(test)]
    pub fn time_boxes(&mut self, n: u32) -> Result<std::time::Duration, String> {
        let i = self.cmds.len() - 1;
        let t = std::time::Instant::now();
        for _ in 0..n {
            self.submit(i)?;
        }
        Ok(t.elapsed() / n)
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

    /// An update that was sent: its pixels in the marked cells (`CELL`×`CELL`,
    /// row by row) are driven as
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
        if marked.len() != self.cells {
            return Err("the cells marked do not fit the frame".into());
        }
        let mask = unsafe { std::slice::from_raw_parts_mut(self.buffers[GMASK].ptr as *mut u32, self.cells) };
        mask.iter_mut().zip(marked).for_each(|(m, &b)| *m = b as u32);
        self.write_params([0.0, 1.0, 0.0, 1.0], mode as u32, kind);
        self.submit(2)
    }

    /// The error against the newest frame again, after drives and no new
    /// frame (None until one has been rendered).
    pub fn ghost_tick(&mut self) -> Result<Option<Vec<u32>>, String> {
        let Some(kind) = self.desired.filter(|_| self.ghost.is_some()) else { return Ok(None) };
        // No frame here: nothing is asked of the changed-cells layer.
        self.ask = Ask::default();
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
        let ccols = grid.cell_cols();
        let mask = |m: usize| -> Vec<bool> { (0..grid.cells()).map(|t| t % m != 1).collect() };
        let (a_gray, a) = blocks(w, h, 3);
        let (b_gray, b) = blocks(w, h, 4);
        let ga = gpu.gray_bgrx(&bgrx(&a_gray), FULL).expect("frame a");
        assert_eq!(nibbles(&ga.bytes, w, h), a);
        assert_eq!(ga.ghost.expect("errors"), cpu.errors(&a, cols, rows), "against the assumed white panel");
        // A was sent everywhere, fast; B is then rendered and measured against what the panel got.
        let all = vec![true; grid.cells()];
        gpu.ghost_drive(Some(&ga.bytes), Kind::Gray4, Drive::Fast, &all).unwrap();
        cpu.drive(&a, Drive::Fast, &all, ccols);
        let gb = gpu.gray_bgrx(&bgrx(&b_gray), FULL).expect("frame b");
        assert_eq!(gb.ghost.expect("errors"), cpu.errors(&b, cols, rows));
        // B goes out in some cells fast, then a settle in others; measured between frames.
        gpu.ghost_drive(Some(&gb.bytes), Kind::Gray4, Drive::Fast, &mask(2)).unwrap();
        cpu.drive(&b, Drive::Fast, &mask(2), ccols);
        gpu.ghost_drive(None, Kind::Gray4, Drive::Settle, &mask(3)).unwrap();
        cpu.drive(&b, Drive::Settle, &mask(3), ccols);
        assert_eq!(gpu.ghost_tick().unwrap().expect("errors"), cpu.errors(&b, cols, rows));
        assert_eq!(gpu.ghost_cells().expect("cells"), cpu.cell_errors(&b));
        // A reset forgets it all.
        gpu.ghost_drive(None, Kind::Gray4, Drive::Reset, &all).unwrap();
        cpu.drive(&b, Drive::Reset, &all, ccols);
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
        let ccols = grid.cell_cols();
        let all = vec![true; grid.cells()];
        // A dark bar, moved a column at a time: its outline moves with it.
        let bar = |x0: usize| -> Vec<u8> { (0..w * h).map(|i| if (x0..x0 + 20).contains(&(i % w)) && (10..30).contains(&(i / w)) { 20 } else { 255 }).collect() };
        let levels = |o: &Outline| -> Vec<u8> { unpack(&o.bits, w, h).iter().map(|&v| if v == 255 { 15 } else { 0 }).collect() };
        let mut last = vec![];
        for x0 in [10, 11, 12] {
            let o = gpu.outline_bgrx(&bgrx(&bar(x0)), FULL).expect("frame");
            let l = levels(&o);
            assert_eq!(o.ghost.expect("errors"), cpu.errors(&l, cols, rows));
            gpu.ghost_drive(Some(&o.bits), Kind::Bits, Drive::Fast, &all).unwrap();
            cpu.drive(&l, Drive::Fast, &all, ccols);
            last = l;
        }
        let errs = gpu.ghost_tick().unwrap().expect("errors");
        assert_eq!(errs, cpu.errors(&last, cols, rows));
        assert!(errs.iter().any(|&e| e > 0), "a moved outline leaves residue");
        let cells = cpu.cell_errors(&last);
        assert_eq!(gpu.ghost_cells().expect("cells"), cells);
        // The boxes the GPU draws around the ghosts are the reference's, from the same cells.
        let set: Vec<bool> = cells.iter().map(|&e| e >= ghost::CELL_ERR).collect();
        let want: Vec<Rect> = tiles::cell_boxes(&set, grid.cell_cols(), h.div_ceil(CELL)).into_iter().map(|b| tiles::box_rect(b, w, h)).collect();
        assert_eq!(sorted(gpu.ghost_boxes().expect("boxes")), sorted(want));
        gpu.ghost_drive(None, Kind::Bits, Drive::Redrive, &all).unwrap();
        cpu.drive(&last, Drive::Redrive, &all, ccols);
        let after = gpu.ghost_tick().unwrap().expect("errors");
        assert_eq!(after, cpu.errors(&last, cols, rows));
        assert!(after.iter().sum::<u32>() < errs.iter().sum::<u32>());
    }

    fn sorted(mut v: Vec<Rect>) -> Vec<Rect> {
        v.sort();
        v
    }

    /// The boxes of a gray frame in which, against a white one, a pixel of
    /// every cell marked in `set` (`cw` cells a row) differs.
    fn boxes_of_cells(gpu: &mut Gpu, w: usize, h: usize, set: &[bool], cw: usize) -> Vec<Rect> {
        gpu.gray_bgrx(&bgrx(&vec![255u8; w * h]), FULL).expect("white frame");
        let mut img = vec![255u8; w * h];
        for (c, _) in set.iter().enumerate().filter(|(_, s)| **s) {
            let (x, y) = (((c % cw) * CELL + 3).min(w - 1), ((c / cw) * CELL + 5).min(h - 1));
            img[y * w + x] = 0;
        }
        gpu.gray_bgrx(&bgrx(&img), FULL).expect("changed frame").boxes.expect("boxes")
    }

    /// Whether every marked cell lies inside one of the rectangles.
    fn covers_cells(rects: &[Rect], set: &[bool], cw: usize, w: usize, h: usize) -> bool {
        set.iter().enumerate().filter(|(_, s)| **s).all(|(c, _)| {
            let (x, y) = (((c % cw) * CELL + 3).min(w - 1), ((c / cw) * CELL + 5).min(h - 1));
            rects.iter().any(|&(rx, ry, rw, rh)| x >= rx && x < rx + rw && y >= ry && y < ry + rh)
        })
    }

    /// A pseudo-random sequence for the maps below.
    fn lcg(mut s: u32) -> impl FnMut() -> u32 {
        move || {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            s >> 8
        }
    }

    /// The GPU's boxes are the CPU reference's, to the box, on structured and
    /// random maps of changed cells; and they always cover every changed cell.
    #[test]
    fn the_gpu_boxes_match_the_cpu_reference() {
        let (w, h) = (203, 157);
        let Some(mut gpu) = gpu(w, h) else { return };
        let (cw, ch) = (w.div_ceil(CELL), h.div_ceil(CELL));
        let from_px = |x0: usize, y0: usize, bw: usize, bh: usize| -> Vec<bool> { (0..cw * ch).map(|c| { let (x, y) = ((c % cw) * CELL, (c / cw) * CELL); x + CELL > x0 && x < x0 + bw && y + CELL > y0 && y < y0 + bh }).collect() };
        let or = |a: Vec<bool>, b: Vec<bool>| -> Vec<bool> { a.iter().zip(&b).map(|(&a, &b)| a || b).collect() };
        let mut rand = lcg(5);
        let mut maps: Vec<(&str, Vec<bool>)> = vec![
            ("empty", vec![false; cw * ch]),
            ("cursor", from_px(50, 61, 12, 19)),
            ("two blobs", or(from_px(10, 10, 12, 19), from_px(170, 120, 30, 20))),
            ("gap of one cell", or(from_px(16, 16, 8, 8), from_px(32, 16, 8, 8))),
            ("diagonal", (0..cw * ch).map(|c| { let (x, y) = (c % cw, c / cw); (x * ch / cw).abs_diff(y) <= 1 }).collect()),
            ("full", vec![true; cw * ch]),
            ("single cell at the corner", from_px(w - 1, h - 1, 1, 1)),
        ];
        for density in [3, 10, 30, 60] {
            maps.push(("random", (0..cw * ch).map(|_| rand() % 100 < density).collect()));
        }
        for (name, set) in &maps {
            let got = boxes_of_cells(&mut gpu, w, h, set, cw);
            let want: Vec<Rect> = tiles::cell_boxes(set, cw, ch).into_iter().map(|b| tiles::box_rect(b, w, h)).collect();
            assert!(covers_cells(&got, set, cw, w, h), "{name}: a changed cell is not covered");
            assert!(got.iter().all(|&(x, _, _, _)| x % CELL == 0), "{name}: not on whole bytes");
            if want.len() < BOXES {
                assert_eq!(sorted(got), sorted(want), "{name}");
            } else {
                assert_eq!(got.len(), BOXES, "{name}");
            }
        }
    }

    /// More components than the list holds: the last box is the union of the
    /// rest, so nothing changed is left out and the list stays short.
    #[test]
    fn too_many_boxes_are_folded_into_the_last_one() {
        let (w, h) = (400, 300);
        let Some(mut gpu) = gpu(w, h) else { return };
        let (cw, ch) = (w.div_ceil(CELL), h.div_ceil(CELL));
        // A cell every third one in both directions: each its own component.
        let set: Vec<bool> = (0..cw * ch).map(|c| (c % cw) % 3 == 0 && (c / cw) % 3 == 0).collect();
        assert!(set.iter().filter(|&&s| s).count() > 3 * BOXES);
        let got = boxes_of_cells(&mut gpu, w, h, &set, cw);
        assert_eq!(got.len(), BOXES);
        assert!(covers_cells(&got, &set, cw, w, h));
        // The CPU's last step makes of them the few rectangles an update takes, still covering.
        let rects = tiles::finish(got);
        assert!(rects.len() <= 8 && covers_cells(&rects, &set, cw, w, h));
    }

    /// The history answers for any base up to 31 frames back, and with the
    /// boxes of exactly the frames since it; not for one it does not know.
    #[test]
    fn boxes_are_against_the_frame_the_sender_has() {
        let (w, h) = (96, 64);
        let Some(mut gpu) = gpu(w, h) else { return };
        let cw = w.div_ceil(CELL);
        // Frame n adds a black pixel in cell n (row 0).
        let mut img = vec![255u8; w * h];
        let frame = |gpu: &mut Gpu, img: &[u8], seq: u64, base: Option<u64>| gpu.gray_frame(&bgrx(img), FULL, Some(Frame { seq, base })).expect("frame").boxes;
        assert_eq!(frame(&mut gpu, &img, 1, None), None);
        let mut seen = vec![];
        for n in 2..=9u64 {
            img[(n as usize - 2) % cw * CELL + 1] = 0;
            seen.push(sorted(frame(&mut gpu, &img, n, Some(n - 1)).expect("boxes")));
        }
        // Each frame alone is one cell wide.
        assert!(seen.iter().all(|b| b.len() == 1 && b[0].2 == CELL && b[0].3 == CELL));
        // A base further back gives the union of the cells since: frames 3..=10 changed cells 1..=8.
        img[8 % cw * CELL + 1] = 0;
        let b = frame(&mut gpu, &img, 10, Some(2)).expect("boxes");
        assert_eq!(tiles::finish(b), vec![(CELL, 0, 8 * CELL, CELL)]);
        // And for a base that is not in the run, or too old, no answer.
        img[9 % cw * CELL + 1] = 0;
        assert_eq!(frame(&mut gpu, &img, 11, Some(0)), None);
        img[10 % cw * CELL + 1] = 0;
        assert_eq!(frame(&mut gpu, &img, 50, Some(11)), None, "a gap in the numbers starts a new run");
        // A frame of a new run has nothing to be compared with, and the next one is answered again.
        assert!(frame(&mut gpu, &img, 51, Some(50)).is_some());
    }

    /// The cursor, a dragged window and a diagonal stroke, in rectangles
    /// (printed with `--nocapture`), against what the 32x32 tile grid gave.
    #[test]
    fn a_cursor_a_dragged_window_and_a_diagonal_stroke_in_rectangles() {
        let (w, h) = (400, 300);
        let Some(mut gpu) = gpu(w, h) else { return };
        let grid = tiles::Grid::new(w, h);
        let fill = |img: &mut [u8], (x0, y0, bw, bh): Rect, v: u8| {
            for y in y0..(y0 + bh).min(h) {
                img[y * w..][x0..(x0 + bw).min(w)].fill(v);
            }
        };
        let mut seq = 0;
        // Frames `a` then `b`: the rectangles of the change, and the tile grid's.
        let mut change = |gpu: &mut Gpu, a: &[u8], b: &[u8]| -> (Vec<Rect>, Vec<Rect>) {
            seq += 2;
            let ga = gpu.gray_frame(&bgrx(a), FULL, Some(Frame { seq: seq - 1, base: Some(seq - 2) })).expect("frame a");
            let gb = gpu.gray_frame(&bgrx(b), FULL, Some(Frame { seq, base: Some(seq - 1) })).expect("frame b");
            let tile_rects = grid.rects(&grid.dirty_packed(&ga.bytes, &gb.bytes, 2));
            (tiles::finish(gb.boxes.expect("boxes")), tile_rects)
        };
        let total = |r: &[Rect]| r.iter().map(|&(_, _, w, h)| w * h).sum::<usize>();
        let white = vec![255u8; w * h];
        change(&mut gpu, &white, &white);
        // The cursor (12x19, black) moves 4 right and 2 down, across a tile corner.
        let (mut a, mut b) = (white.clone(), white.clone());
        fill(&mut a, (124, 122, 12, 19), 0);
        fill(&mut b, (128, 124, 12, 19), 0);
        let (cursor, cursor_tiles) = change(&mut gpu, &a, &b);
        eprintln!("cursor: {} rect(s), {} px (tiles: {} rect(s), {} px)", cursor.len(), total(&cursor), cursor_tiles.len(), total(&cursor_tiles));
        assert!(cursor.len() == 1 && total(&cursor) <= 24 * 32 && total(&cursor) * 3 < total(&cursor_tiles));
        // A window of noise (200x150) dragged 24 px right and 8 down.
        let mut rand = lcg(9);
        let noise: Vec<u8> = (0..200 * 150).map(|_| (rand() % 256) as u8).collect();
        let put = |img: &mut [u8], (x0, y0): (usize, usize)| {
            for y in 0..150 {
                img[(y0 + y) * w + x0..][..200].copy_from_slice(&noise[y * 200..][..200]);
            }
        };
        let (mut a, mut b) = (white.clone(), white.clone());
        put(&mut a, (40, 30));
        put(&mut b, (64, 38));
        let (window, window_tiles) = change(&mut gpu, &a, &b);
        eprintln!("window drag: {} rect(s), {} px (tiles: {} rect(s), {} px; old and new windows {} px)", window.len(), total(&window), window_tiles.len(), total(&window_tiles), 2 * 200 * 150);
        assert!(window.len() == 1 && total(&window) <= 232 * 168);
        // A 3 px diagonal stroke across the screen.
        let mut stroke = white.clone();
        for i in 0..300 {
            fill(&mut stroke, (i * 399 / 299, i, 3, 3), 0);
        }
        let (diag, diag_tiles) = change(&mut gpu, &white, &stroke);
        eprintln!("diagonal stroke: {} rect(s), {} px (tiles: {} rect(s), {} px; whole box {} px)", diag.len(), total(&diag), diag_tiles.len(), total(&diag_tiles), w * h);
        assert!(diag.len() <= 8 && total(&diag) * 2 < w * h);
        // A short stroke (50 cells long) is cut into more, smaller rectangles before the final merge.
        let mut short = white.clone();
        for i in 0..100 {
            fill(&mut short, (100 + i, 100 + i, 3, 3), 0);
        }
        let (diag2, diag2_tiles) = change(&mut gpu, &white, &short);
        eprintln!("short diagonal: {} rect(s), {} px (tiles: {} rect(s), {} px; whole box {} px)", diag2.len(), total(&diag2), diag2_tiles.len(), total(&diag2_tiles), 104 * 104);
        assert!(total(&diag2) * 2 < 104 * 104);
    }

    /// In the edge view too the boxes cover every pixel that differs from the
    /// previous frame.
    #[test]
    fn the_edge_view_boxes_cover_what_changed() {
        let (w, h) = (203, 157);
        let Some(mut gpu) = gpu(w, h) else { return };
        let bar = |x0: usize, y0: usize| -> Vec<u8> { (0..w * h).map(|i| if (x0..x0 + 20).contains(&(i % w)) && (y0..y0 + 25).contains(&(i / w)) { 20 } else { 255 }).collect() };
        let mut last = gpu.outline_bgrx(&bgrx(&bar(10, 10)), FULL).expect("frame").bits;
        for (x0, y0) in [(11, 10), (14, 12), (100, 90), (100, 91), (101, 91)] {
            let o = gpu.outline_bgrx(&bgrx(&bar(x0, y0)), FULL).expect("frame");
            let boxes = o.boxes.expect("boxes");
            let (before, after) = (unpack(&last, w, h), unpack(&o.bits, w, h));
            let differ: Vec<usize> = (0..w * h).filter(|&i| before[i] != after[i]).collect();
            assert!(!differ.is_empty());
            assert!(differ.iter().all(|&i| boxes.iter().any(|&(x, y, bw, bh)| i % w >= x && i % w < x + bw && i / w >= y && i / w < y + bh)), "a changed pixel is outside the boxes {boxes:?}");
            last = o.bits;
        }
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

    /// Time of the passes that find the rectangles on a full-size frame, by
    /// the difference between a frame that asks for them and one that does
    /// not, for a cursor, scattered text-like cells, a diagonal stroke and a
    /// full-screen change (`cargo test --release -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn times_the_rectangle_passes() {
        let (w, h) = (1872usize, 1404usize);
        let (cw, ch) = (w.div_ceil(CELL), h.div_ceil(CELL));
        let Some(mut gpu) = gpu(w, h) else { return };
        let mut rand = lcg(3);
        let cases: Vec<(&str, Vec<bool>)> = vec![
            ("empty", vec![false; cw * ch]),
            ("cursor", (0..cw * ch).map(|c| (100..102).contains(&(c % cw)) && (50..53).contains(&(c / cw))).collect()),
            ("scattered 5%", (0..cw * ch).map(|_| rand() % 100 < 5).collect()),
            ("diagonal", (0..cw * ch).map(|c| ((c % cw) * ch / cw).abs_diff(c / cw) <= 1).collect()),
            ("full screen", vec![true; cw * ch]),
        ];
        let white = bgrx(&vec![255u8; w * h]);
        for (name, set) in &cases {
            let mut img = vec![255u8; w * h];
            for (c, _) in set.iter().enumerate().filter(|(_, s)| **s) {
                img[((c / cw) * CELL + 2).min(h - 1) * w + ((c % cw) * CELL + 2).min(w - 1)] = 0;
            }
            let changed = bgrx(&img);
            // The white frame, then the changed one against it; the passes alone on the maps that leaves.
            gpu.gray_frame(&white, FULL, Some(Frame { seq: 10, base: None })).expect("frame");
            let boxes = gpu.gray_frame(&changed, FULL, Some(Frame { seq: 11, base: Some(10) })).expect("frame").boxes.map_or(0, |b| b.len());
            gpu.time_boxes(5).expect("warm up");
            let per = gpu.time_boxes(200).expect("passes");
            eprintln!("{name}: {per:?} for the rectangle passes alone (one submit and wait included), {boxes} boxes, on {}", gpu.name());
        }
        gpu.gray_frame(&white, FULL, Some(Frame { seq: 12, base: Some(11) })).expect("frame");
        let t = std::time::Instant::now();
        for i in 0..30 {
            gpu.gray_frame(&white, FULL, Some(Frame { seq: 13 + i, base: Some(12 + i) })).expect("frame");
        }
        eprintln!("a whole gray frame, for comparison: {:?}", t.elapsed() / 30);
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
