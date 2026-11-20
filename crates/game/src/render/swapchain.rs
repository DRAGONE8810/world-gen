//! Swapchain, depth buffer and framebuffers. Recreated whenever the window
//! is resized or vsync is toggled.

use ash::vk;
use gpu_allocator::vulkan::{Allocation, AllocationCreateDesc, AllocationScheme, Allocator};
use gpu_allocator::MemoryLocation;
use std::sync::{Arc, Mutex};

pub fn is_srgb(format: vk::Format) -> bool {
    matches!(format, vk::Format::B8G8R8A8_SRGB | vk::Format::R8G8B8A8_SRGB | vk::Format::A8B8G8R8_SRGB_PACK32)
}

#[derive(Default)]
pub struct Swapchain {
    pub handle: vk::SwapchainKHR,
    pub color_format: vk::SurfaceFormatKHR,
    pub depth_format: vk::Format,
    pub extent: vk::Extent2D,
    pub views: Vec<vk::ImageView>,
    pub framebuffers: Vec<vk::Framebuffer>,
    /// One "rendering finished" semaphore per swapchain image: presentation
    /// of image i may still be using the semaphore of image i.
    pub render_finished: Vec<vk::Semaphore>,
    depth_image: vk::Image,
    depth_view: vk::ImageView,
    depth_alloc: Option<Allocation>,
}

impl Swapchain {
    pub unsafe fn choose_formats(
        instance: &ash::Instance,
        surface_loader: &ash::khr::surface::Instance,
        physical: vk::PhysicalDevice,
        surface: vk::SurfaceKHR,
    ) -> Result<(vk::SurfaceFormatKHR, vk::Format), vk::Result> {
        let formats = surface_loader.get_physical_device_surface_formats(physical, surface)?;
        let color = formats
            .iter()
            .copied()
            .find(|f| is_srgb(f.format) && f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR)
            .unwrap_or(formats[0]);
        let depth = [vk::Format::D32_SFLOAT, vk::Format::X8_D24_UNORM_PACK32, vk::Format::D16_UNORM]
            .into_iter()
            .find(|&f| {
                instance
                    .get_physical_device_format_properties(physical, f)
                    .optimal_tiling_features
                    .contains(vk::FormatFeatureFlags::DEPTH_STENCIL_ATTACHMENT)
            })
            .unwrap_or(vk::Format::D16_UNORM);
        Ok((color, depth))
    }

    #[allow(clippy::too_many_arguments)]
    pub unsafe fn new(
        device: &ash::Device,
        surface_loader: &ash::khr::surface::Instance,
        loader: &ash::khr::swapchain::Device,
        allocator: &Arc<Mutex<Allocator>>,
        physical: vk::PhysicalDevice,
        surface: vk::SurfaceKHR,
        render_pass: vk::RenderPass,
        color_format: vk::SurfaceFormatKHR,
        depth_format: vk::Format,
        size: (u32, u32),
        vsync: bool,
        old: vk::SwapchainKHR,
    ) -> Result<Self, vk::Result> {
        let caps = surface_loader.get_physical_device_surface_capabilities(physical, surface)?;
        let extent = if caps.current_extent.width != u32::MAX {
            caps.current_extent
        } else {
            vk::Extent2D {
                width: size.0.clamp(caps.min_image_extent.width, caps.max_image_extent.width),
                height: size.1.clamp(caps.min_image_extent.height, caps.max_image_extent.height),
            }
        };
        let modes = surface_loader.get_physical_device_surface_present_modes(physical, surface)?;
        let present_mode = if vsync {
            vk::PresentModeKHR::FIFO // always available
        } else if modes.contains(&vk::PresentModeKHR::MAILBOX) {
            vk::PresentModeKHR::MAILBOX
        } else if modes.contains(&vk::PresentModeKHR::IMMEDIATE) {
            vk::PresentModeKHR::IMMEDIATE
        } else {
            vk::PresentModeKHR::FIFO
        };
        let mut image_count = caps.min_image_count + 1;
        if caps.max_image_count > 0 {
            image_count = image_count.min(caps.max_image_count);
        }
        let composite = if caps.supported_composite_alpha.contains(vk::CompositeAlphaFlagsKHR::OPAQUE) {
            vk::CompositeAlphaFlagsKHR::OPAQUE
        } else {
            vk::CompositeAlphaFlagsKHR::INHERIT
        };
        let handle = loader.create_swapchain(
            &vk::SwapchainCreateInfoKHR::default()
                .surface(surface)
                .min_image_count(image_count)
                .image_format(color_format.format)
                .image_color_space(color_format.color_space)
                .image_extent(extent)
                .image_array_layers(1)
                .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
                .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
                .pre_transform(caps.current_transform)
                .composite_alpha(composite)
                .present_mode(present_mode)
                .clipped(true)
                .old_swapchain(old),
            None,
        )?;
        let images = loader.get_swapchain_images(handle)?;
        let mut views = Vec::new();
        for &image in &images {
            views.push(create_view(device, image, color_format.format, vk::ImageAspectFlags::COLOR)?);
        }

        // Depth buffer.
        let depth_image = device.create_image(
            &vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(depth_format)
                .extent(vk::Extent3D { width: extent.width, height: extent.height, depth: 1 })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT)
                .initial_layout(vk::ImageLayout::UNDEFINED),
            None,
        )?;
        let requirements = device.get_image_memory_requirements(depth_image);
        let depth_alloc = allocator
            .lock()
            .unwrap()
            .allocate(&AllocationCreateDesc {
                name: "depth",
                requirements,
                location: MemoryLocation::GpuOnly,
                linear: false,
                allocation_scheme: AllocationScheme::GpuAllocatorManaged,
            })
            .map_err(|_| vk::Result::ERROR_OUT_OF_DEVICE_MEMORY)?;
        device.bind_image_memory(depth_image, depth_alloc.memory(), depth_alloc.offset())?;
        let depth_view = create_view(device, depth_image, depth_format, vk::ImageAspectFlags::DEPTH)?;

        let mut framebuffers = Vec::new();
        let mut render_finished = Vec::new();
        for &view in &views {
            let attachments = [view, depth_view];
            framebuffers.push(
                device.create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(render_pass)
                        .attachments(&attachments)
                        .width(extent.width)
                        .height(extent.height)
                        .layers(1),
                    None,
                )?,
            );
            render_finished.push(device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)?);
        }

        Ok(Self {
            handle,
            color_format,
            depth_format,
            extent,
            views,
            framebuffers,
            render_finished,
            depth_image,
            depth_view,
            depth_alloc: Some(depth_alloc),
        })
    }

    pub unsafe fn destroy(
        mut self,
        device: &ash::Device,
        loader: &ash::khr::swapchain::Device,
        allocator: &Arc<Mutex<Allocator>>,
    ) {
        for fb in self.framebuffers.drain(..) {
            device.destroy_framebuffer(fb, None);
        }
        for v in self.views.drain(..) {
            device.destroy_image_view(v, None);
        }
        for s in self.render_finished.drain(..) {
            device.destroy_semaphore(s, None);
        }
        if self.depth_view != vk::ImageView::null() {
            device.destroy_image_view(self.depth_view, None);
            device.destroy_image(self.depth_image, None);
        }
        if let Some(a) = self.depth_alloc.take() {
            let _ = allocator.lock().unwrap().free(a);
        }
        if self.handle != vk::SwapchainKHR::null() {
            loader.destroy_swapchain(self.handle, None);
        }
    }
}

unsafe fn create_view(
    device: &ash::Device,
    image: vk::Image,
    format: vk::Format,
    aspect: vk::ImageAspectFlags,
) -> Result<vk::ImageView, vk::Result> {
    device.create_image_view(
        &vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: aspect,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            }),
        None,
    )
}
