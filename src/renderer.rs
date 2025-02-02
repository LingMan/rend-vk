use core::panic;
use std::{
    alloc::Layout,
    collections::HashMap,
    ffi::CStr,
    mem::align_of,
    sync::atomic::{AtomicU64, Ordering},
};

use ash::{
    extensions::{
        ext::{self, DebugUtils},
        khr,
    },
    util::Align,
    vk, Entry,
};
use bitvec::vec::BitVec;

use crate::{
    buffer::{DeviceAllocator, DeviceSlice},
    context::{self, ExtensionContext, VulkanContext},
    debug::{self, DebugContext},
    format::Format,
    pipeline::{
        self,
        attachment::Attachment,
        sampler::{Sampler, SamplerKey},
        Pipeline,
    },
    render_task::{RenderTask, TaskKind},
    shader_resource::{ResourceKind, SingleResource},
    swapchain,
    texture::{MipMap, Texture},
    UsedAsIndex,
};

#[derive(Clone)]
pub struct MeshBuffer {
    pub vertices: DeviceSlice,
    pub normals: DeviceSlice,
    pub tex_coords: DeviceSlice,
    pub indices: DeviceSlice,
    pub count: u32,
}

pub struct Renderer {
    pub vulkan_context: Box<context::VulkanContext>,
    swapchain_context: Box<swapchain::SwapchainContext>,
    debug_context: Option<Box<debug::DebugContext>>,
    pipeline: Box<Pipeline>,
    general_allocator: Box<DeviceAllocator>,
    descriptor_allocator: Box<DeviceAllocator>,
    mesh_buffers_by_id: HashMap<u32, MeshBuffer>,
    textures_by_id: HashMap<u32, Texture>,
    shader_resources_by_kind: HashMap<ResourceKind, SingleResource>,
    batches_by_task_type: Vec<Vec<RenderTask>>,
    mesh_buffer_ids: BitVec,

    optimal_transition_queue: Vec<u32>,
    ongoing_optimal_transitions: Vec<(u32, u64)>,

    present_queue: vk::Queue,

    pool: vk::CommandPool,
    draw_command_buffer: vk::CommandBuffer,
    _setup_command_buffer: vk::CommandBuffer,

    present_complete_semaphore: vk::Semaphore,
    rendering_complete_semaphore: vk::Semaphore,
    pass_timeline_semaphore: vk::Semaphore,

    draw_commands_reuse_fence: vk::Fence,
    setup_commands_reuse_fence: vk::Fence,

    current_frame: AtomicU64,
}

impl Renderer {
    pub const ID_TEST_TRIANGLE: u32 = 0;
    pub const MAX_SAMPLERS: u32 = 32;

    pub fn destroy(&mut self) {
        log::trace!("destroying renderer...");
        self.pipeline.destroy(&self.vulkan_context.device);
        for e in [&self.general_allocator, &self.descriptor_allocator] {
            e.destroy(&self.vulkan_context.device);
        }
        unsafe {
            let destroy_semaphore = |s| self.vulkan_context.device.destroy_semaphore(s, None);
            let destroy_fence = |s| self.vulkan_context.device.destroy_fence(s, None);
            self.vulkan_context.device.device_wait_idle().unwrap();
            destroy_semaphore(self.present_complete_semaphore);
            destroy_semaphore(self.rendering_complete_semaphore);
            destroy_semaphore(self.pass_timeline_semaphore);
            destroy_fence(self.draw_commands_reuse_fence);
            destroy_fence(self.setup_commands_reuse_fence);
            self.vulkan_context
                .device
                .destroy_command_pool(self.pool, None);
            self.swapchain_context.destroy(&self.vulkan_context);
            self.vulkan_context.device.destroy_device(None);
        }
        // TODO: Read about Drop
        if self.debug_context.is_some() {
            let d = self.debug_context.as_mut().unwrap();
            d.destroy();
        }
        unsafe { self.vulkan_context.instance.destroy_instance(None) };
        log::trace!("renderer destroyed!");
    }

    pub fn add_task_to_queue(&mut self, task: RenderTask) {
        if let Some(batch) = self.batches_by_task_type.get_mut(task.kind as usize) {
            batch.push(task)
        }
    }

    pub fn try_get_sampler(&self, key: SamplerKey) -> Option<u8> {
        match self.pipeline.samplers_by_key.get(&key) {
            Some(s) => Some(s.position),
            None => None,
        }
    }

    pub fn get_sampler(&mut self, key: SamplerKey) -> u8 {
        let id = self.try_get_sampler(key);
        if id.is_some() {
            return id.unwrap();
        }
        //  Sampler for this key not found, generate one
        let id = self.pipeline.samplers_by_key.len() as u32;
        let name = format!("{}", id);
        let sampler = Sampler::of_key(&self.vulkan_context, name, key, id as u8);
        let samplers_by_key = &mut self.pipeline.samplers_by_key;
        //  store it for later querying
        samplers_by_key.insert(key, sampler.clone());
        let sampler_descriptors = &mut self.pipeline.sampler_descriptors;
        // Write its descriptor into the GPU for later shader usage
        sampler_descriptors.place_sampler_at(
            id,
            0,
            sampler.sampler,
            &self.vulkan_context.extension.descriptor_buffer,
        );
        sampler_descriptors.into_device_single_at(0, id);
        // Return the ID for referencing on the client side
        return id as u8;
    }

    pub fn fetch_mesh(&self, id: u32) -> Option<&MeshBuffer> {
        self.mesh_buffers_by_id.get(&id)
    }

    pub fn fetch_mesh_or_fail(&self, id: u32) -> &MeshBuffer {
        self.fetch_mesh(id)
            .unwrap_or_else(|| panic!("couldn't find mesh with id {}", id))
    }

    pub fn free_mesh(&mut self, id: u32) {
        let mesh = self
            .mesh_buffers_by_id
            .remove(&id)
            .unwrap_or_else(|| panic!("couldn't find mesh with id {}", id));
        let free_if_not_empty = |v: &DeviceSlice| {
            if v.size > 0 {
                self.general_allocator.free(v.clone());
            }
        };
        free_if_not_empty(&mesh.vertices);
        free_if_not_empty(&mesh.normals);
        free_if_not_empty(&mesh.tex_coords);
        free_if_not_empty(&mesh.indices);
        self.mesh_buffer_ids.set(id as usize, false);
    }

    pub fn gen_mesh(
        &mut self,
        vertices_size: u32,
        normals_size: u32,
        tex_coords_size: u32,
        indices_size: u32,
        count: u32,
    ) -> u32 {
        let alloc_or_empty = |size: u32, purpose: &str| {
            if size > 0 {
                self.general_allocator
                    .alloc(size as u64)
                    .unwrap_or_else(|| {
                        panic!("couldnt allocate '{}' buffer of size {}", purpose, size)
                    })
            } else {
                DeviceSlice::empty()
            }
        };

        let vertices = alloc_or_empty(vertices_size, "vertex");
        let normals = alloc_or_empty(normals_size, "normal");
        let tex_coords = alloc_or_empty(tex_coords_size, "tex_coord");
        let indices = alloc_or_empty(indices_size, "index");
        // Reserve mesh id
        let mesh_id = self
            .mesh_buffer_ids
            .first_zero()
            .expect("ran out of mesh ids!") as u32;

        self.mesh_buffer_ids.set(mesh_id as usize, true);

        self.mesh_buffers_by_id.insert(
            mesh_id,
            MeshBuffer {
                vertices,
                normals,
                tex_coords,
                indices,
                count,
            },
        );

        return mesh_id;
    }

    pub fn fetch_texture(&self, id: u32) -> Option<&Texture> {
        self.textures_by_id.get(&id)
    }

    pub fn gen_texture(
        &mut self,
        name: String,
        format: crate::format::Format,
        mip_maps: &[MipMap],
        staging_size: u32,
    ) -> u32 {
        // Reserve texture id
        let texture_id = self.pipeline.image_descriptors.next_free() as u32;
        let staging = if staging_size > 0 {
            Some(Box::new(
                self.general_allocator
                    .alloc(staging_size as u64)
                    .unwrap_or_else(|| {
                        panic!(
                            "can't allocate staging buffer of size {} for {}",
                            name, staging_size
                        )
                    }),
            ))
        } else {
            None
        };
        let texture = crate::texture::make(
            &self.vulkan_context,
            texture_id,
            name,
            mip_maps,
            format,
            false,
            staging,
        );
        // Generate descriptor and place it in the image descriptor array buffer
        self.pipeline.image_descriptors.place_image_at(
            texture_id,
            0,
            vk::DescriptorImageInfo {
                image_view: texture.view,
                image_layout: vk::ImageLayout::READ_ONLY_OPTIMAL,
                ..Default::default()
            },
            &self.vulkan_context.extension.descriptor_buffer,
        );
        self.textures_by_id.insert(texture_id, texture);
        return texture_id;
    }

    pub fn queue_texture_for_uploading(&mut self, id: u32) {
        if !self.textures_by_id.contains_key(&id) {
            panic!("missing texture with id {}", id);
        }
        self.optimal_transition_queue.push(id);
    }

    pub fn is_texture_uploaded(&self, id: u32) -> bool {
        let texture = self
            .textures_by_id
            .get(&id)
            .unwrap_or_else(|| panic!("missing texture with id {}", id));
        return texture.staging.is_none();
    }

    pub fn place_shader_resource(&mut self, kind: ResourceKind, item: SingleResource) {
        self.shader_resources_by_kind.insert(kind, item);
    }

    pub fn render(&mut self) {
        unsafe {
            let (present_index, _) = self
                .vulkan_context
                .extension
                .swapchain
                .acquire_next_image(
                    self.swapchain_context.swapchain,
                    std::u64::MAX,
                    self.present_complete_semaphore,
                    vk::Fence::null(),
                )
                .unwrap();
            let default_attachment =
                self.swapchain_context.attachments[present_index as usize].clone();
            self.record_submit_commandbuffer(
                self.draw_command_buffer,
                self.draw_commands_reuse_fence,
                self.present_queue,
                &[vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT],
                &[self.present_complete_semaphore],
                &[self.rendering_complete_semaphore],
                &default_attachment,
            );
            let wait_semaphores = [self.rendering_complete_semaphore];
            let swapchains = [self.swapchain_context.swapchain];
            let image_indices = [present_index];
            let present_info = vk::PresentInfoKHR::builder()
                .wait_semaphores(&wait_semaphores)
                .swapchains(&swapchains)
                .image_indices(&image_indices);
            self.vulkan_context
                .extension
                .swapchain
                .queue_present(self.present_queue, &present_info)
                .unwrap();
            // Next frame ID
            self.incr_current_frame();
            // Clear batch queues for next frame
            for batch in &mut self.batches_by_task_type {
                batch.clear();
            }
        }
    }

    fn incr_current_frame(&self) -> u64 {
        self.current_frame.fetch_add(1, Ordering::Relaxed)
    }

    fn get_current_frame(&self) -> u64 {
        self.current_frame.load(Ordering::Relaxed)
    }

    fn process_stages(&mut self, default_attachment: &Attachment) {
        let current_frame = self.get_current_frame();
        let sampler_descriptors = self.pipeline.sampler_descriptors.clone();
        let image_descriptors = self.pipeline.image_descriptors.clone();
        let buffer_allocator = self.general_allocator.clone();
        let total_stages = self.pipeline.total_stages();
        let pipeline = &mut self.pipeline;

        if !self.ongoing_optimal_transitions.is_empty() {
            let current_timeline_counter = unsafe {
                self.vulkan_context
                    .device
                    .get_semaphore_counter_value(self.pass_timeline_semaphore)
                    .unwrap()
            };
            let prev_len = self.ongoing_optimal_transitions.len();
            self.ongoing_optimal_transitions.retain(|e| {
                if e.1 > current_timeline_counter {
                    return true;
                }
                let texture = &mut self.textures_by_id.get_mut(&e.0).unwrap();
                // Free the staging buffer after it has been used
                match &texture.staging {
                    Some(staging) => {
                        let device = staging.as_ref().clone();
                        self.general_allocator.free(device);
                    }
                    _ => panic!(
                        "staging buffer for texture {} {} is missing!",
                        texture.id, texture.name
                    ),
                }
                // Set staging to None to mark the texture as "uploaded"
                texture.staging = None;
                return false;
            });
            if prev_len != self.ongoing_optimal_transitions.len() {
                // Update the descriptors on the device
                pipeline.image_descriptors.into_device();
            }
        }

        for texture_id in self.optimal_transition_queue.drain(..) {
            let texture = &self.textures_by_id[&texture_id];
            texture.transition_to_optimal(&self.vulkan_context, self.draw_command_buffer);
            self.ongoing_optimal_transitions
                .push((texture_id, pipeline.signal_value_for(current_frame + 1, 0)))
        }

        for stage in pipeline.stages.iter_mut() {
            stage.wait_for_previous_frame(
                &self.vulkan_context.device,
                current_frame,
                total_stages,
                self.pass_timeline_semaphore,
            );
            stage.render(
                &self.vulkan_context,
                &self.batches_by_task_type,
                &self.mesh_buffers_by_id,
                &self.shader_resources_by_kind,
                &sampler_descriptors,
                &image_descriptors,
                &buffer_allocator,
                self.draw_command_buffer,
                default_attachment,
            );
            stage.signal_next_frame(
                &self.vulkan_context.device,
                current_frame,
                total_stages,
                self.pass_timeline_semaphore,
                self.present_queue,
            );
        }
    }

    fn record_submit_commandbuffer(
        &mut self,
        command_buffer: vk::CommandBuffer,
        command_buffer_reuse_fence: vk::Fence,
        submit_queue: vk::Queue,
        wait_mask: &[vk::PipelineStageFlags],
        wait_semaphores: &[vk::Semaphore],
        signal_semaphores: &[vk::Semaphore],
        default_attachment: &Attachment,
    ) {
        unsafe {
            self.vulkan_context
                .device
                .wait_for_fences(&[command_buffer_reuse_fence], true, std::u64::MAX)
                .expect("fence wait failed!");

            self.vulkan_context
                .device
                .reset_fences(&[command_buffer_reuse_fence])
                .expect("fence reset failed!");

            self.vulkan_context
                .device
                .reset_command_buffer(
                    command_buffer,
                    vk::CommandBufferResetFlags::RELEASE_RESOURCES,
                )
                .expect("reset command buffer failed!");

            let command_buffer_begin_info = vk::CommandBufferBeginInfo::builder()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);

            self.vulkan_context
                .device
                .begin_command_buffer(command_buffer, &command_buffer_begin_info)
                .expect("begin commandbuffer failed!");

            self.process_stages(default_attachment);

            self.vulkan_context
                .device
                .end_command_buffer(command_buffer)
                .expect("end command buffer failed!");

            let command_buffers = vec![command_buffer];

            let submit_info = vk::SubmitInfo::builder()
                .wait_semaphores(wait_semaphores)
                .wait_dst_stage_mask(wait_mask)
                .command_buffers(&command_buffers)
                .signal_semaphores(signal_semaphores);

            self.vulkan_context
                .device
                .queue_submit(
                    submit_queue,
                    &[submit_info.build()],
                    command_buffer_reuse_fence,
                )
                .expect("queue submit failed!");
        }
    }
}

pub fn make_renderer<F>(
    is_vsync_enabled: bool,
    is_debug_enabled: bool,
    is_validation_layer_enabled: bool,
    instance_extensions: &[*const i8],
    create_surface: F,
) -> Renderer
where
    F: FnOnce(&ash::Entry, &ash::Instance, *mut vk::SurfaceKHR) -> vk::Result,
{
    log::trace!("entering make_renderer");

    log::trace!("creating entry...");
    let entry = Entry::linked();
    log::trace!("entry created!");
    log::trace!("creating instance...");
    let instance = make_instance(
        &entry,
        instance_extensions,
        is_debug_enabled,
        is_validation_layer_enabled,
    );
    log::trace!("instance created!");

    let debug_context = if is_debug_enabled {
        Some(Box::new(DebugContext::new(&entry, &instance)))
    } else {
        None
    };

    let debug_utils_ext = if is_debug_enabled {
        Some(DebugUtils::new(&entry, &instance))
    } else {
        None
    };
    log::trace!("creating surface...");
    let surface_layout = Layout::new::<vk::SurfaceKHR>();
    let surface = unsafe { std::alloc::alloc(surface_layout) as *mut vk::SurfaceKHR };
    let create_surface_result = create_surface(&entry, &instance, surface);
    if create_surface_result != vk::Result::SUCCESS {
        panic!("error creating surface: {}", create_surface_result);
    }
    let surface = unsafe { *surface };
    log::trace!("surface created!");
    let surface_extension = khr::Surface::new(&entry, &instance);
    // let make_surface = func: unsafe extern "C" fn(u64, *mut c_void),
    log::trace!("selecting physical device...");
    let (physical_device, queue_family_index) =
        select_physical_device(&instance, &surface_extension, surface);
    log::trace!("physical device selected!");
    log::trace!("creating device...");
    let device = make_device(
        &instance,
        physical_device,
        queue_family_index,
        is_debug_enabled,
    );
    log::trace!("device created!");

    let swapchain_extension = ash::extensions::khr::Swapchain::new(&instance, &device);
    let descriptor_buffer_ext = ash::extensions::ext::DescriptorBuffer::new(&instance, &device);

    log::trace!("creating command buffers...");
    let present_queue = unsafe { device.get_device_queue(queue_family_index, 0) };

    let pool_create_info = vk::CommandPoolCreateInfo::builder()
        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
        .queue_family_index(queue_family_index);

    let pool = unsafe { device.create_command_pool(&pool_create_info, None).unwrap() };

    let command_buffer_allocate_info = vk::CommandBufferAllocateInfo::builder()
        .command_buffer_count(2)
        .command_pool(pool)
        .level(vk::CommandBufferLevel::PRIMARY);

    let command_buffers = unsafe {
        device
            .allocate_command_buffers(&command_buffer_allocate_info)
            .unwrap()
    };
    let setup_command_buffer = command_buffers[0];
    let draw_command_buffer = command_buffers[1];
    log::trace!("command buffers created!");

    log::trace!("creating fences...");
    let fence_create_info = vk::FenceCreateInfo::builder().flags(vk::FenceCreateFlags::SIGNALED);
    let draw_commands_reuse_fence = unsafe {
        device
            .create_fence(&fence_create_info, None)
            .expect("Create fence failed.")
    };
    let setup_commands_reuse_fence = unsafe {
        device
            .create_fence(&fence_create_info, None)
            .expect("Create fence failed.")
    };
    log::trace!("fences created!");

    log::trace!("creating semaphores...");
    let semaphore_create_info = vk::SemaphoreCreateInfo::default();
    let present_complete_semaphore = unsafe {
        device
            .create_semaphore(&semaphore_create_info, None)
            .unwrap()
    };
    let rendering_complete_semaphore = unsafe {
        device
            .create_semaphore(&semaphore_create_info, None)
            .unwrap()
    };
    let mut timeline_semaphore_type_create_info = vk::SemaphoreTypeCreateInfo::builder()
        .initial_value(0)
        .semaphore_type(vk::SemaphoreType::TIMELINE)
        .build();
    let timeline_semaphore_create_info = vk::SemaphoreCreateInfo::builder()
        .push_next(&mut timeline_semaphore_type_create_info)
        .build();
    let pass_timeline_semaphore = unsafe {
        device
            .create_semaphore(&timeline_semaphore_create_info, None)
            .unwrap()
    };
    log::trace!("semaphores created!");

    let mem_props = unsafe { instance.get_physical_device_memory_properties(physical_device) };

    let vulkan_context = VulkanContext {
        entry,
        device,
        instance,
        physical_device,
        memory_properties: mem_props,
        extension: ExtensionContext {
            descriptor_buffer: descriptor_buffer_ext,
            debug_utils: debug_utils_ext,
            swapchain: swapchain_extension,
            surface: surface_extension,
        },
    };

    log::trace!("creating allocators...");
    let mut general_allocator = DeviceAllocator::new_general(&vulkan_context, 64 * 1024 * 1024);
    let mut descriptor_allocator = DeviceAllocator::new_descriptor(&vulkan_context, 1024 * 1024);
    log::trace!("allocators created!");

    log::trace!("creating swapchain...");
    let swapchain_context =
        swapchain::SwapchainContext::make(&vulkan_context, surface, is_vsync_enabled);
    log::trace!("swapchain created!");

    log::trace!("creating pipeline...");
    let pip = pipeline::file::Pipeline::load(
        &vulkan_context,
        &mut descriptor_allocator,
        swapchain_context.attachments[0].clone(),
        is_validation_layer_enabled,
        Some("pipeline.json"),
    );
    log::trace!("pipeline created!");

    log::trace!("creating test triangle...");
    let test_triangle = make_test_triangle(&mut general_allocator);

    let mut mesh_buffer_ids = BitVec::repeat(false, 1024);
    let mut mesh_buffers_by_id = HashMap::new();
    mesh_buffer_ids.set(Renderer::ID_TEST_TRIANGLE as usize, true);
    mesh_buffers_by_id.insert(Renderer::ID_TEST_TRIANGLE, test_triangle);

    let textures_by_id = HashMap::new();

    log::trace!("test triangle created!");
    let mut batches_by_task_type = Vec::with_capacity(TaskKind::MAX_SIZE + 1);
    (0..TaskKind::MAX_LEN).for_each(|_| {
        batches_by_task_type.push(Vec::new());
    });

    log::trace!("finishing renderer...");
    let mut renderer = Renderer {
        pipeline: Box::new(pip),
        batches_by_task_type,
        debug_context,
        swapchain_context: Box::new(swapchain_context),
        vulkan_context: Box::new(vulkan_context),
        general_allocator: Box::new(general_allocator),
        descriptor_allocator: Box::new(descriptor_allocator),
        mesh_buffers_by_id,
        mesh_buffer_ids,
        textures_by_id,
        draw_command_buffer,
        present_queue,
        _setup_command_buffer: setup_command_buffer,
        rendering_complete_semaphore,
        pass_timeline_semaphore,
        present_complete_semaphore,
        setup_commands_reuse_fence,
        draw_commands_reuse_fence,
        pool,
        optimal_transition_queue: Vec::new(),
        ongoing_optimal_transitions: Vec::new(),
        shader_resources_by_kind: HashMap::new(),
        current_frame: AtomicU64::new(0),
    };
    // Reserve the texture ID 0 with an empty texture
    renderer.gen_texture(
        "default_texture".to_string(),
        Format::R8G8B8A8_UNORM,
        &[MipMap {
            index: 0,
            size: 4,
            offset: 0,
            width: 1,
            height: 1,
        }],
        0,
    );
    log::trace!("renderer finished!");
    return renderer;
}

pub fn make_device(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    queue_family_index: u32,
    is_debug_enabled: bool,
) -> ash::Device {
    let mut device_extension_names_raw = vec![
        khr::Swapchain::name().as_ptr(),
        ext::DescriptorBuffer::name().as_ptr(),
    ];
    let non_semantic_info_name =
        CStr::from_bytes_with_nul(b"VK_KHR_shader_non_semantic_info\0").unwrap();
    if is_debug_enabled {
        device_extension_names_raw.push(non_semantic_info_name.as_ptr());
    }
    let features = vk::PhysicalDeviceFeatures {
        shader_clip_distance: 1,
        ..Default::default()
    };
    let mut features12 = vk::PhysicalDeviceVulkan12Features {
        descriptor_indexing: 1,
        timeline_semaphore: 1,
        buffer_device_address: 1,
        scalar_block_layout: 1,
        runtime_descriptor_array: 1,
        shader_sampled_image_array_non_uniform_indexing: 1,
        ..Default::default()
    };
    let mut features13 = vk::PhysicalDeviceVulkan13Features {
        dynamic_rendering: 1,
        synchronization2: 1,
        ..Default::default()
    };
    let mut descriptor_buffer_feature = vk::PhysicalDeviceDescriptorBufferFeaturesEXT {
        descriptor_buffer: 1,
        ..Default::default()
    };
    let mut features2 = vk::PhysicalDeviceFeatures2::builder()
        .features(features)
        .push_next(&mut features12)
        .push_next(&mut features13)
        .push_next(&mut descriptor_buffer_feature)
        .build();

    let priorities = [1.0];

    let queue_info = vk::DeviceQueueCreateInfo::builder()
        .queue_family_index(queue_family_index)
        .queue_priorities(&priorities)
        .build();

    let device_create_info = vk::DeviceCreateInfo::builder()
        .queue_create_infos(std::slice::from_ref(&queue_info))
        .enabled_extension_names(&device_extension_names_raw)
        .push_next(&mut features2)
        .build();

    log::info!("initializing Device...");
    let device: ash::Device = unsafe {
        instance
            .create_device(physical_device, &device_create_info, None)
            .expect("couldn't create the device!")
    };
    log::info!("device initialized!");
    return device;
}

pub fn make_instance(
    entry: &ash::Entry,
    extensions: &[*const i8],
    is_debug_enabled: bool,
    is_validation_layer_enabled: bool,
) -> ash::Instance {
    let app_name = CStr::from_bytes_with_nul(b"rend-vk\0").unwrap();

    let mut layers_names_raw = vec![];

    let validation_layer_name =
        CStr::from_bytes_with_nul(b"VK_LAYER_KHRONOS_validation\0").unwrap();
    if is_debug_enabled && is_validation_layer_enabled {
        layers_names_raw.push(validation_layer_name.as_ptr());
    }

    let mut instance_extensions = extensions.to_vec();
    if is_debug_enabled {
        instance_extensions.push(DebugUtils::name().as_ptr());
    }

    let appinfo = vk::ApplicationInfo::builder()
        .application_name(app_name)
        .application_version(0)
        .engine_name(app_name)
        .engine_version(0)
        .api_version(vk::make_api_version(0, 1, 3, 0));

    let mut create_info = vk::InstanceCreateInfo::builder()
        .application_info(&appinfo)
        .enabled_layer_names(&layers_names_raw)
        .enabled_extension_names(&instance_extensions);

    let enabled_validation_features = [vk::ValidationFeatureEnableEXT::DEBUG_PRINTF];
    let mut validation_features_ext = vk::ValidationFeaturesEXT::builder()
        .enabled_validation_features(&enabled_validation_features)
        .build();

    if is_debug_enabled {
        create_info = create_info.push_next(&mut validation_features_ext);
    }

    log::info!("initializing Instance...");
    let instance: ash::Instance = unsafe {
        entry
            .create_instance(&create_info, None)
            .expect("instance creation error!")
    };
    log::info!("instance initialized!");
    return instance;
}

pub fn select_physical_device(
    instance: &ash::Instance,
    surface_extension: &khr::Surface,
    window_surface: vk::SurfaceKHR,
) -> (vk::PhysicalDevice, u32) {
    let devices = unsafe {
        instance
            .enumerate_physical_devices()
            .expect("Physical device error")
    };
    devices
        .iter()
        .find_map(|pdevice| {
            let properties = unsafe { instance.get_physical_device_properties(*pdevice) };
            let is_discrete = vk::PhysicalDeviceType::DISCRETE_GPU == properties.device_type;
            if !is_discrete {
                return None;
            }
            unsafe {
                instance
                    .get_physical_device_queue_family_properties(*pdevice)
                    .iter()
                    .enumerate()
                    .find_map(|(index, info)| {
                        let supports_graphic_and_surface =
                            info.queue_flags.contains(vk::QueueFlags::GRAPHICS)
                                && surface_extension
                                    .get_physical_device_surface_support(
                                        *pdevice,
                                        index as u32,
                                        window_surface,
                                    )
                                    .unwrap();
                        if supports_graphic_and_surface {
                            Some((*pdevice, index as u32))
                        } else {
                            None
                        }
                    })
            }
        })
        .expect("Couldn't find a suitable physical device!")
}

fn make_test_triangle(buffer_allocator: &mut DeviceAllocator) -> MeshBuffer {
    #[derive(Clone, Debug, Copy)]
    struct Attrib3f {
        pub values: [f32; 3],
    }
    #[derive(Clone, Debug, Copy)]
    struct Attrib2f {
        pub values: [f32; 2],
    }
    let vertices = [
        Attrib3f {
            values: [-1.0, 1.0, 0.0],
        },
        Attrib3f {
            values: [1.0, 1.0, 0.0],
        },
        Attrib3f {
            values: [0.0, -1.0, 0.0],
        },
    ];
    let tex_coords = [
        Attrib2f { values: [0.0, 0.0] },
        Attrib2f { values: [1.0, 0.0] },
        Attrib2f { values: [1.0, 1.0] },
    ];
    let normals = [
        Attrib3f {
            values: [0.0, 1.0, 0.0],
        },
        Attrib3f {
            values: [1.0, 1.0, 0.0],
        },
        Attrib3f {
            values: [1.0, 0.0, 0.0],
        },
    ];

    fn alloc_and_copy<T: std::marker::Copy>(
        elements: &[T],
        buffer_allocator: &mut DeviceAllocator,
    ) -> DeviceSlice {
        let buffer = buffer_allocator
            .alloc(std::mem::size_of_val(&elements) as u64)
            .expect("couldn't allocate index buffer");
        let mut slice = unsafe {
            Align::new(
                buffer.addr,
                align_of::<u32>() as u64,
                buffer_allocator.buffer.alignment,
            )
        };
        slice.copy_from_slice(elements);
        buffer
    }

    let vertex_buffer = alloc_and_copy(&vertices, buffer_allocator);
    let normal_buffer = alloc_and_copy(&normals, buffer_allocator);
    let tex_coord_buffer = alloc_and_copy(&tex_coords, buffer_allocator);

    MeshBuffer {
        vertices: vertex_buffer,
        indices: DeviceSlice::empty(),
        tex_coords: tex_coord_buffer,
        normals: normal_buffer,
        count: vertices.len() as u32,
    }
}
