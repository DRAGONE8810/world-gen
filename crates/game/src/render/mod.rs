//! Vulkan renderer (via `ash`).
//!
//! Deliberately simple: one render pass, one pipeline for terrain, egui drawn
//! on top. What matters for scale is already here:
//!
//! * **GPU-only vertex memory.** Chunk meshes are copied through a staging
//!   buffer into device-local memory, the fastest memory for the GPU to read.
//! * **Camera-relative rendering.** Positions sent to the GPU are relative to
//!   the camera, computed in `f64` on the CPU (see `terrain.vert`).
//! * **Frustum culling.** Chunks outside the view are never submitted.
//! * **Frames in flight.** The CPU records frame N+1 while the GPU draws
//!   frame N; buffers are only freed once the GPU is done with them.

mod swapchain;

use crate::camera::{Camera, Frustum};
use crate::lod::NodeKey;
use crate::mesh::{MeshData, Vertex};
use ash::vk;
use glam::{Vec3, Vec4};
use gpu_allocator::vulkan::{Allocation, AllocationCreateDesc, AllocationScheme, Allocator, AllocatorCreateDesc};
use gpu_allocator::MemoryLocation;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use std::collections::HashMap;
use std::ffi::{c_char, CStr};
use std::mem::ManuallyDrop;
use std::sync::{Arc, Mutex};
use swapchain::Swapchain;

const FRAMES_IN_FLIGHT: usize = 2;
/// Upper bound on chunk uploads per frame, to keep frame times steady.
/// Mesh uploads recorded per frame. The streamer never hands over more,
/// so everything it calls ready is on the GPU by the next draw.
pub const MAX_UPLOADS_PER_FRAME: usize = 48;

static VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/terrain.vert.spv"));
static FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/terrain.frag.spv"));

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct PushConstants {
    view_proj: [f32; 16],
    offset_fog: [f32; 4],
    fog_color: [f32; 4],
}

struct GpuBuffer {
    buffer: vk::Buffer,
    alloc: Allocation,
}

struct GpuMesh {
    vertices: GpuBuffer,
    indices: GpuBuffer,
    index_count: u32,
    min_y: f32,
    max_y: f32,
}

struct Frame {
    cmd: vk::CommandBuffer,
    image_available: vk::Semaphore,
    in_flight: vk::Fence,
    /// Buffers that were in use when this frame was recorded and can be
    /// freed once its fence signals.
    garbage: Vec<GpuBuffer>,
}

/// Everything the UI layer hands to the renderer for one frame.
pub struct UiFrame<'a> {
    pub primitives: &'a [egui::ClippedPrimitive],
    pub textures_delta: &'a egui::TexturesDelta,
    pub pixels_per_point: f32,
}

pub struct FrameStats {
    pub drawn_chunks: usize,
    pub drawn_triangles: u64,
    pub gpu_meshes: usize,
}

pub struct Renderer {
    _entry: ash::Entry,
    instance: ash::Instance,
    debug: Option<(ash::ext::debug_utils::Instance, vk::DebugUtilsMessengerEXT)>,
    surface_loader: ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
    physical: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    allocator: ManuallyDrop<Arc<Mutex<Allocator>>>,
    swapchain_loader: ash::khr::swapchain::Device,
    swapchain: Swapchain,
    render_pass: vk::RenderPass,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    command_pool: vk::CommandPool,
    upload_pool: vk::CommandPool,
    frames: Vec<Frame>,
    frame_index: usize,
    meshes: HashMap<NodeKey, GpuMesh>,
    pending_uploads: Vec<(NodeKey, MeshData)>,
    pending_garbage: Vec<GpuBuffer>,
    egui: ManuallyDrop<egui_ash_renderer::Renderer<egui_ash_renderer::allocator::GpuAllocator>>,
    vsync: bool,
    needs_recreate: bool,
    pub gpu_name: String,
}

impl Renderer {
    pub fn new(window: &winit::window::Window, vsync: bool) -> Result<Self, String> {
        unsafe { Self::new_inner(window, vsync) }.map_err(|e| e.to_string())
    }

    unsafe fn new_inner(window: &winit::window::Window, vsync: bool) -> Result<Self, Box<dyn std::error::Error>> {
        let entry = ash::Entry::load().map_err(|e| {
            format!("Vulkan is not available on this computer ({e}). Install or update your graphics driver.")
        })?;
        let display = window.display_handle()?.as_raw();
        let window_handle = window.window_handle()?.as_raw();

        // ---- Instance -----------------------------------------------------
        let validation = std::env::var("WG_VALIDATION").is_ok_and(|v| v == "1");
        let available_exts = entry.enumerate_instance_extension_properties(None)?;
        let has_ext = |name: &CStr| available_exts.iter().any(|e| e.extension_name_as_c_str() == Ok(name));
        let mut extensions: Vec<*const c_char> = ash_window::enumerate_required_extensions(display)?.to_vec();
        let mut flags = vk::InstanceCreateFlags::empty();
        // macOS (MoltenVK) exposes Vulkan as a "portability" implementation.
        if has_ext(ash::khr::portability_enumeration::NAME) {
            extensions.push(ash::khr::portability_enumeration::NAME.as_ptr());
            extensions.push(ash::khr::get_physical_device_properties2::NAME.as_ptr());
            flags |= vk::InstanceCreateFlags::ENUMERATE_PORTABILITY_KHR;
        }
        let mut layers: Vec<*const c_char> = Vec::new();
        let validation_layer = c"VK_LAYER_KHRONOS_validation";
        if validation {
            let have_layer = entry
                .enumerate_instance_layer_properties()?
                .iter()
                .any(|l| l.layer_name_as_c_str() == Ok(validation_layer));
            if have_layer && has_ext(ash::ext::debug_utils::NAME) {
                layers.push(validation_layer.as_ptr());
                extensions.push(ash::ext::debug_utils::NAME.as_ptr());
            } else {
                log::warn!("WG_VALIDATION=1 but the Khronos validation layer is not installed");
            }
        }
        let app = vk::ApplicationInfo::default()
            .application_name(c"world-gen")
            .engine_name(c"world-gen")
            .api_version(vk::API_VERSION_1_1);
        let instance = entry.create_instance(
            &vk::InstanceCreateInfo::default()
                .application_info(&app)
                .enabled_extension_names(&extensions)
                .enabled_layer_names(&layers)
                .flags(flags),
            None,
        )?;

        let debug = if !layers.is_empty() {
            let loader = ash::ext::debug_utils::Instance::new(&entry, &instance);
            let info = vk::DebugUtilsMessengerCreateInfoEXT::default()
                .message_severity(
                    vk::DebugUtilsMessageSeverityFlagsEXT::WARNING | vk::DebugUtilsMessageSeverityFlagsEXT::ERROR,
                )
                .message_type(
                    vk::DebugUtilsMessageTypeFlagsEXT::GENERAL
                        | vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION
                        | vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE,
                )
                .pfn_user_callback(Some(debug_callback));
            let messenger = loader.create_debug_utils_messenger(&info, None)?;
            Some((loader, messenger))
        } else {
            None
        };

        // ---- Surface & physical device ------------------------------------
        let surface_loader = ash::khr::surface::Instance::new(&entry, &instance);
        let surface = ash_window::create_surface(&entry, &instance, display, window_handle, None)?;

        let mut best: Option<(vk::PhysicalDevice, u32, i32)> = None;
        for pd in instance.enumerate_physical_devices()? {
            let props = instance.get_physical_device_properties(pd);
            let families = instance.get_physical_device_queue_family_properties(pd);
            let Some(family) = families.iter().enumerate().position(|(i, f)| {
                f.queue_flags.contains(vk::QueueFlags::GRAPHICS)
                    && surface_loader.get_physical_device_surface_support(pd, i as u32, surface).unwrap_or(false)
            }) else {
                continue;
            };
            let score = match props.device_type {
                vk::PhysicalDeviceType::DISCRETE_GPU => 3,
                vk::PhysicalDeviceType::INTEGRATED_GPU => 2,
                vk::PhysicalDeviceType::VIRTUAL_GPU => 1,
                _ => 0,
            };
            if best.is_none_or(|b| score > b.2) {
                best = Some((pd, family as u32, score));
            }
        }
        let (physical, queue_family, _) = best.ok_or("No GPU with Vulkan graphics + presentation support was found")?;
        let props = instance.get_physical_device_properties(physical);
        let gpu_name = props.device_name_as_c_str().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        log::info!("Using GPU: {gpu_name}");

        // ---- Logical device -----------------------------------------------
        let dev_exts = instance.enumerate_device_extension_properties(physical)?;
        let mut device_extensions = vec![ash::khr::swapchain::NAME.as_ptr()];
        if dev_exts.iter().any(|e| e.extension_name_as_c_str() == Ok(ash::khr::portability_subset::NAME)) {
            device_extensions.push(ash::khr::portability_subset::NAME.as_ptr());
        }
        let priorities = [1.0];
        let queue_info =
            [vk::DeviceQueueCreateInfo::default().queue_family_index(queue_family).queue_priorities(&priorities)];
        let device = instance.create_device(
            physical,
            &vk::DeviceCreateInfo::default()
                .queue_create_infos(&queue_info)
                .enabled_extension_names(&device_extensions),
            None,
        )?;
        let queue = device.get_device_queue(queue_family, 0);

        let allocator = Arc::new(Mutex::new(Allocator::new(&AllocatorCreateDesc {
            instance: instance.clone(),
            device: device.clone(),
            physical_device: physical,
            debug_settings: Default::default(),
            buffer_device_address: false,
            allocation_sizes: Default::default(),
        })?));

        // ---- Swapchain + render pass --------------------------------------
        let swapchain_loader = ash::khr::swapchain::Device::new(&instance, &device);
        let size = window.inner_size();
        let (color_format, depth_format) = Swapchain::choose_formats(&instance, &surface_loader, physical, surface)?;
        let render_pass = create_render_pass(&device, color_format.format, depth_format)?;
        let swapchain = Swapchain::new(
            &device,
            &surface_loader,
            &swapchain_loader,
            &allocator,
            physical,
            surface,
            render_pass,
            color_format,
            depth_format,
            (size.width, size.height),
            vsync,
            vk::SwapchainKHR::null(),
        )?;

        let (pipeline_layout, pipeline) = create_pipeline(&device, render_pass)?;

        // ---- Per-frame resources ------------------------------------------
        let command_pool = device.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .queue_family_index(queue_family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
            None,
        )?;
        let upload_pool = device.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .queue_family_index(queue_family)
                .flags(vk::CommandPoolCreateFlags::TRANSIENT | vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
            None,
        )?;
        let cmds = device.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(command_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(FRAMES_IN_FLIGHT as u32),
        )?;
        let mut frames = Vec::new();
        for cmd in cmds {
            frames.push(Frame {
                cmd,
                image_available: device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)?,
                in_flight: device
                    .create_fence(&vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED), None)?,
                garbage: Vec::new(),
            });
        }

        let egui = egui_ash_renderer::Renderer::with_gpu_allocator(
            allocator.clone(),
            device.clone(),
            egui_ash_renderer::RenderMode::RenderPass(render_pass),
            egui_ash_renderer::Options {
                in_flight_frames: FRAMES_IN_FLIGHT,
                enable_depth_test: false,
                enable_depth_write: false,
                srgb_framebuffer: swapchain::is_srgb(color_format.format),
            },
        )?;

        Ok(Self {
            _entry: entry,
            instance,
            debug,
            surface_loader,
            surface,
            physical,
            device,
            queue,
            allocator: ManuallyDrop::new(allocator),
            swapchain_loader,
            swapchain,
            render_pass,
            pipeline_layout,
            pipeline,
            command_pool,
            upload_pool,
            frames,
            frame_index: 0,
            meshes: HashMap::new(),
            pending_uploads: Vec::new(),
            pending_garbage: Vec::new(),
            egui: ManuallyDrop::new(egui),
            vsync,
            needs_recreate: false,
            gpu_name,
        })
    }

    pub fn resized(&mut self) {
        self.needs_recreate = true;
    }

    pub fn set_vsync(&mut self, vsync: bool) {
        if vsync != self.vsync {
            self.vsync = vsync;
            self.needs_recreate = true;
        }
    }

    pub fn aspect(&self) -> f32 {
        let e = self.swapchain.extent;
        e.width.max(1) as f32 / e.height.max(1) as f32
    }

    /// Queue a chunk or LOD tile mesh for upload (replaces any previous mesh).
    pub fn upload_mesh(&mut self, pos: NodeKey, mesh: MeshData) {
        self.pending_uploads.push((pos, mesh));
    }

    pub fn remove_mesh(&mut self, pos: NodeKey) {
        self.pending_uploads.retain(|(p, _)| *p != pos);
        if let Some(m) = self.meshes.remove(&pos) {
            self.pending_garbage.push(m.vertices);
            self.pending_garbage.push(m.indices);
        }
    }

    /// Drops every chunk mesh (used when leaving a world).
    pub fn clear_meshes(&mut self) {
        self.pending_uploads.clear();
        let keys: Vec<_> = self.meshes.keys().copied().collect();
        for k in keys {
            self.remove_mesh(k);
        }
    }

    fn create_buffer(&self, size: u64, usage: vk::BufferUsageFlags, location: MemoryLocation, name: &str) -> GpuBuffer {
        unsafe {
            let buffer = self
                .device
                .create_buffer(
                    &vk::BufferCreateInfo::default().size(size).usage(usage).sharing_mode(vk::SharingMode::EXCLUSIVE),
                    None,
                )
                .expect("create_buffer");
            let requirements = self.device.get_buffer_memory_requirements(buffer);
            let alloc = self
                .allocator
                .lock()
                .unwrap()
                .allocate(&AllocationCreateDesc {
                    name,
                    requirements,
                    location,
                    linear: true,
                    allocation_scheme: AllocationScheme::GpuAllocatorManaged,
                })
                .expect("GPU out of memory");
            self.device.bind_buffer_memory(buffer, alloc.memory(), alloc.offset()).expect("bind_buffer_memory");
            GpuBuffer { buffer, alloc }
        }
    }

    fn free_buffer(&self, b: GpuBuffer) {
        unsafe { self.device.destroy_buffer(b.buffer, None) };
        let _ = self.allocator.lock().unwrap().free(b.alloc);
    }

    /// Records copies for queued meshes into `cmd`. Staging buffers go to
    /// the current frame's garbage list.
    fn record_uploads(&mut self, cmd: vk::CommandBuffer) {
        let n = self.pending_uploads.len().min(MAX_UPLOADS_PER_FRAME);
        if n == 0 {
            return;
        }
        let batch: Vec<_> = self.pending_uploads.drain(..n).collect();
        let mut garbage = Vec::new();
        for (pos, mesh) in batch {
            if let Some(old) = self.meshes.remove(&pos) {
                garbage.push(old.vertices);
                garbage.push(old.indices);
            }
            if mesh.indices.is_empty() {
                continue;
            }
            let vbytes: &[u8] = bytemuck::cast_slice(&mesh.vertices);
            let ibytes: &[u8] = bytemuck::cast_slice(&mesh.indices);
            let total = (vbytes.len() + ibytes.len()) as u64;
            let mut staging =
                self.create_buffer(total, vk::BufferUsageFlags::TRANSFER_SRC, MemoryLocation::CpuToGpu, "staging");
            {
                let dst = staging.alloc.mapped_slice_mut().expect("staging is host visible");
                dst[..vbytes.len()].copy_from_slice(vbytes);
                dst[vbytes.len()..total as usize].copy_from_slice(ibytes);
            }
            let vertices = self.create_buffer(
                vbytes.len() as u64,
                vk::BufferUsageFlags::VERTEX_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
                MemoryLocation::GpuOnly,
                "chunk vertices",
            );
            let indices = self.create_buffer(
                ibytes.len() as u64,
                vk::BufferUsageFlags::INDEX_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
                MemoryLocation::GpuOnly,
                "chunk indices",
            );
            unsafe {
                self.device.cmd_copy_buffer(
                    cmd,
                    staging.buffer,
                    vertices.buffer,
                    &[vk::BufferCopy { src_offset: 0, dst_offset: 0, size: vbytes.len() as u64 }],
                );
                self.device.cmd_copy_buffer(
                    cmd,
                    staging.buffer,
                    indices.buffer,
                    &[vk::BufferCopy { src_offset: vbytes.len() as u64, dst_offset: 0, size: ibytes.len() as u64 }],
                );
            }
            garbage.push(staging);
            self.meshes.insert(
                pos,
                GpuMesh {
                    vertices,
                    indices,
                    index_count: mesh.indices.len() as u32,
                    min_y: mesh.min_y,
                    max_y: mesh.max_y,
                },
            );
        }
        // Make the copies visible to the vertex input stage.
        let barrier = vk::MemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::VERTEX_ATTRIBUTE_READ | vk::AccessFlags::INDEX_READ);
        unsafe {
            self.device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::VERTEX_INPUT,
                vk::DependencyFlags::empty(),
                &[barrier],
                &[],
                &[],
            );
        }
        self.frames[self.frame_index].garbage.extend(garbage);
    }

    fn recreate_swapchain(&mut self, size: (u32, u32)) -> Result<(), vk::Result> {
        unsafe {
            self.device.device_wait_idle()?;
            let old = std::mem::take(&mut self.swapchain);
            let new = Swapchain::new(
                &self.device,
                &self.surface_loader,
                &self.swapchain_loader,
                &self.allocator,
                self.physical,
                self.surface,
                self.render_pass,
                old.color_format,
                old.depth_format,
                size,
                self.vsync,
                old.handle,
            )?;
            old.destroy(&self.device, &self.swapchain_loader, &self.allocator);
            self.swapchain = new;
        }
        self.needs_recreate = false;
        Ok(())
    }

    /// Draws one frame: terrain from `camera`, then the UI on top.
    pub fn draw(
        &mut self,
        window_size: (u32, u32),
        camera: Option<(&Camera, f32 /* fog distance */, &[NodeKey] /* what to draw */)>,
        sky: [f32; 3],
        ui: UiFrame,
    ) -> FrameStats {
        let mut stats = FrameStats { drawn_chunks: 0, drawn_triangles: 0, gpu_meshes: self.meshes.len() };
        if window_size.0 == 0 || window_size.1 == 0 {
            return stats; // minimised
        }
        if self.needs_recreate
            || self.swapchain.extent.width != window_size.0
            || self.swapchain.extent.height != window_size.1
        {
            if let Err(e) = self.recreate_swapchain(window_size) {
                log::error!("swapchain recreation failed: {e}");
                return stats;
            }
        }

        unsafe {
            let f = self.frame_index;
            let fence = self.frames[f].in_flight;
            self.device.wait_for_fences(&[fence], true, u64::MAX).expect("wait_for_fences");

            // The GPU has finished this frame slot: free what it was using.
            let garbage = std::mem::take(&mut self.frames[f].garbage);
            for b in garbage {
                self.free_buffer(b);
            }
            let pending = std::mem::take(&mut self.pending_garbage);
            self.frames[f].garbage.extend(pending);

            let image_index = match self.swapchain_loader.acquire_next_image(
                self.swapchain.handle,
                u64::MAX,
                self.frames[f].image_available,
                vk::Fence::null(),
            ) {
                Ok((i, _)) => i,
                Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                    self.needs_recreate = true;
                    return stats;
                }
                Err(e) => panic!("acquire_next_image: {e}"),
            };
            self.device.reset_fences(&[fence]).expect("reset_fences");

            // egui texture uploads (font atlas etc.) happen before recording.
            for (id, deltas) in &ui.textures_delta.set {
                for delta in deltas {
                    if let Err(e) = self.egui.set_texture(self.queue, self.upload_pool, *id, delta) {
                        log::error!("egui texture upload failed: {e}");
                    }
                }
            }

            let cmd = self.frames[f].cmd;
            self.device.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty()).unwrap();
            self.device
                .begin_command_buffer(
                    cmd,
                    &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .unwrap();

            self.record_uploads(cmd);

            let extent = self.swapchain.extent;
            let lin = sky.map(|c| c.powf(2.2));
            let clear = [
                vk::ClearValue { color: vk::ClearColorValue { float32: [lin[0], lin[1], lin[2], 1.0] } },
                vk::ClearValue { depth_stencil: vk::ClearDepthStencilValue { depth: 0.0, stencil: 0 } }, // reverse-Z
            ];
            self.device.cmd_begin_render_pass(
                cmd,
                &vk::RenderPassBeginInfo::default()
                    .render_pass(self.render_pass)
                    .framebuffer(self.swapchain.framebuffers[image_index as usize])
                    .render_area(vk::Rect2D { offset: vk::Offset2D::default(), extent })
                    .clear_values(&clear),
                vk::SubpassContents::INLINE,
            );
            let viewport = vk::Viewport {
                x: 0.0,
                y: 0.0,
                width: extent.width as f32,
                height: extent.height as f32,
                min_depth: 0.0,
                max_depth: 1.0,
            };
            self.device.cmd_set_viewport(cmd, 0, &[viewport]);
            self.device.cmd_set_scissor(cmd, 0, &[vk::Rect2D { offset: vk::Offset2D::default(), extent }]);

            if let Some((camera, view_distance, draw_list)) = camera {
                // Reverse-Z with an infinite far plane: depth precision
                // is spread evenly over distance, so tiles 4,000 blocks
                // away don't flicker, and nothing is ever clipped far away
                // (fog hides the edge instead).
                let view_proj = camera.view_proj(self.aspect());
                let frustum = Frustum::from_matrix(view_proj);
                self.device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
                for key in draw_list {
                    let Some(mesh) = self.meshes.get(key) else { continue };
                    // Mesh origin relative to the camera, in f64 then f32.
                    let (bx, bz) = key.origin_block();
                    let ox = (bx as f64 - camera.pos.x) as f32;
                    let oy = (-camera.pos.y) as f32;
                    let oz = (bz as f64 - camera.pos.z) as f32;
                    let size = key.size_blocks() as f32;
                    let min = Vec3::new(ox, oy + mesh.min_y, oz);
                    let max = min + Vec3::new(size, mesh.max_y - mesh.min_y, size);
                    if !frustum.intersects_aabb(min, max) {
                        continue;
                    }
                    let pc = PushConstants {
                        view_proj: view_proj.to_cols_array(),
                        offset_fog: Vec4::new(ox, oy, oz, view_distance).to_array(),
                        fog_color: [lin[0], lin[1], lin[2], 1.0],
                    };
                    self.device.cmd_push_constants(
                        cmd,
                        self.pipeline_layout,
                        vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                        0,
                        bytemuck::bytes_of(&pc),
                    );
                    self.device.cmd_bind_vertex_buffers(cmd, 0, &[mesh.vertices.buffer], &[0]);
                    self.device.cmd_bind_index_buffer(cmd, mesh.indices.buffer, 0, vk::IndexType::UINT32);
                    self.device.cmd_draw_indexed(cmd, mesh.index_count, 1, 0, 0, 0);
                    stats.drawn_chunks += 1;
                    stats.drawn_triangles += mesh.index_count as u64 / 3;
                }
            }

            if let Err(e) = self.egui.cmd_draw(cmd, extent, ui.pixels_per_point, ui.primitives) {
                log::error!("egui draw failed: {e}");
            }

            self.device.cmd_end_render_pass(cmd);
            self.device.end_command_buffer(cmd).unwrap();

            let render_finished = self.swapchain.render_finished[image_index as usize];
            let wait = [self.frames[f].image_available];
            let stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
            let signal = [render_finished];
            let cmds = [cmd];
            self.device
                .queue_submit(
                    self.queue,
                    &[vk::SubmitInfo::default()
                        .wait_semaphores(&wait)
                        .wait_dst_stage_mask(&stages)
                        .command_buffers(&cmds)
                        .signal_semaphores(&signal)],
                    fence,
                )
                .expect("queue_submit");

            let swapchains = [self.swapchain.handle];
            let indices = [image_index];
            match self.swapchain_loader.queue_present(
                self.queue,
                &vk::PresentInfoKHR::default().wait_semaphores(&signal).swapchains(&swapchains).image_indices(&indices),
            ) {
                Ok(false) => {}
                Ok(true) | Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => self.needs_recreate = true,
                Err(e) => panic!("queue_present: {e}"),
            }

            if !ui.textures_delta.free.is_empty() {
                // Rare (font atlas rebuilds): simplest safe option is to wait.
                self.device.device_wait_idle().ok();
                for id in &ui.textures_delta.free {
                    self.egui.free_texture(*id).ok();
                }
            }

            self.frame_index = (self.frame_index + 1) % FRAMES_IN_FLIGHT;
        }
        stats.gpu_meshes = self.meshes.len();
        stats
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            self.device.device_wait_idle().ok();
            ManuallyDrop::drop(&mut self.egui);
            let meshes: Vec<_> = self.meshes.drain().map(|(_, m)| m).collect();
            for m in meshes {
                self.free_buffer(m.vertices);
                self.free_buffer(m.indices);
            }
            let mut all_garbage = std::mem::take(&mut self.pending_garbage);
            for f in &mut self.frames {
                all_garbage.append(&mut f.garbage);
            }
            for b in all_garbage {
                self.free_buffer(b);
            }
            for f in &self.frames {
                self.device.destroy_semaphore(f.image_available, None);
                self.device.destroy_fence(f.in_flight, None);
            }
            self.device.destroy_command_pool(self.command_pool, None);
            self.device.destroy_command_pool(self.upload_pool, None);
            self.device.destroy_pipeline(self.pipeline, None);
            self.device.destroy_pipeline_layout(self.pipeline_layout, None);
            std::mem::take(&mut self.swapchain).destroy(&self.device, &self.swapchain_loader, &self.allocator);
            self.device.destroy_render_pass(self.render_pass, None);
            ManuallyDrop::drop(&mut self.allocator);
            self.device.destroy_device(None);
            self.surface_loader.destroy_surface(self.surface, None);
            if let Some((loader, messenger)) = self.debug.take() {
                loader.destroy_debug_utils_messenger(messenger, None);
            }
            self.instance.destroy_instance(None);
        }
    }
}

unsafe extern "system" fn debug_callback(
    severity: vk::DebugUtilsMessageSeverityFlagsEXT,
    _ty: vk::DebugUtilsMessageTypeFlagsEXT,
    data: *const vk::DebugUtilsMessengerCallbackDataEXT<'_>,
    _user: *mut std::ffi::c_void,
) -> vk::Bool32 {
    let msg = unsafe { (*data).message_as_c_str() }.map(|m| m.to_string_lossy().into_owned()).unwrap_or_default();
    if severity.contains(vk::DebugUtilsMessageSeverityFlagsEXT::ERROR) {
        log::error!("[vulkan] {msg}");
    } else {
        log::warn!("[vulkan] {msg}");
    }
    vk::FALSE
}

unsafe fn create_render_pass(
    device: &ash::Device,
    color: vk::Format,
    depth: vk::Format,
) -> Result<vk::RenderPass, vk::Result> {
    let attachments = [
        vk::AttachmentDescription::default()
            .format(color)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(vk::ImageLayout::PRESENT_SRC_KHR),
        vk::AttachmentDescription::default()
            .format(depth)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL),
    ];
    let color_ref = [vk::AttachmentReference { attachment: 0, layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL }];
    let depth_ref =
        vk::AttachmentReference { attachment: 1, layout: vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL };
    let subpass = [vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(&color_ref)
        .depth_stencil_attachment(&depth_ref)];
    // Wait for the previous use of the colour/depth images before writing.
    let deps = [vk::SubpassDependency::default()
        .src_subpass(vk::SUBPASS_EXTERNAL)
        .dst_subpass(0)
        .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT | vk::PipelineStageFlags::LATE_FRAGMENT_TESTS)
        .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT | vk::PipelineStageFlags::EARLY_FRAGMENT_TESTS)
        .src_access_mask(vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE)
        .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE | vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE)];
    device.create_render_pass(
        &vk::RenderPassCreateInfo::default().attachments(&attachments).subpasses(&subpass).dependencies(&deps),
        None,
    )
}

unsafe fn create_pipeline(
    device: &ash::Device,
    render_pass: vk::RenderPass,
) -> Result<(vk::PipelineLayout, vk::Pipeline), vk::Result> {
    let load = |bytes: &[u8]| -> Result<vk::ShaderModule, vk::Result> {
        let words =
            ash::util::read_spv(&mut std::io::Cursor::new(bytes)).map_err(|_| vk::Result::ERROR_INVALID_SHADER_NV)?;
        device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
    };
    let vs = load(VERT_SPV)?;
    let fs = load(FRAG_SPV)?;
    let push = [vk::PushConstantRange {
        stage_flags: vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
        offset: 0,
        size: std::mem::size_of::<PushConstants>() as u32,
    }];
    let layout =
        device.create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default().push_constant_ranges(&push), None)?;

    let stages = [
        vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::VERTEX).module(vs).name(c"main"),
        vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::FRAGMENT).module(fs).name(c"main"),
    ];
    let bindings = [vk::VertexInputBindingDescription {
        binding: 0,
        stride: std::mem::size_of::<Vertex>() as u32,
        input_rate: vk::VertexInputRate::VERTEX,
    }];
    let attributes = [
        vk::VertexInputAttributeDescription {
            location: 0,
            binding: 0,
            format: vk::Format::R32G32B32_SFLOAT,
            offset: 0,
        },
        vk::VertexInputAttributeDescription { location: 1, binding: 0, format: vk::Format::R8G8B8A8_UNORM, offset: 12 },
    ];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
        .vertex_binding_descriptions(&bindings)
        .vertex_attribute_descriptions(&attributes);
    let input_assembly =
        vk::PipelineInputAssemblyStateCreateInfo::default().topology(vk::PrimitiveTopology::TRIANGLE_LIST);
    let viewport = vk::PipelineViewportStateCreateInfo::default().viewport_count(1).scissor_count(1);
    let raster = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::BACK)
        .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
        .line_width(1.0);
    let multisample =
        vk::PipelineMultisampleStateCreateInfo::default().rasterization_samples(vk::SampleCountFlags::TYPE_1);
    let depth = vk::PipelineDepthStencilStateCreateInfo::default()
        .depth_test_enable(true)
        .depth_write_enable(true)
        .depth_compare_op(vk::CompareOp::GREATER_OR_EQUAL); // reverse-Z
    let blend_attachment =
        [vk::PipelineColorBlendAttachmentState::default().color_write_mask(vk::ColorComponentFlags::RGBA)];
    let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachment);
    let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);

    let info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&input_assembly)
        .viewport_state(&viewport)
        .rasterization_state(&raster)
        .multisample_state(&multisample)
        .depth_stencil_state(&depth)
        .color_blend_state(&blend)
        .dynamic_state(&dynamic)
        .layout(layout)
        .render_pass(render_pass)
        .subpass(0);
    let pipeline = device.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None).map_err(|(_, e)| e)?[0];
    device.destroy_shader_module(vs, None);
    device.destroy_shader_module(fs, None);
    Ok((layout, pipeline))
}
