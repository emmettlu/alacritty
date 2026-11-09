//! Vulkan rendering with persistent mapped frame buffers and batched atlas uploads.
use std::{rc::Rc, sync::Arc};
use ash::{vk, Entry};
use crate::{platform::window::Window, renderer::Error};
use super::{atlas::GlyphAtlas, FrameUniforms, RectInstanceData, TextDrawCommand, TextInstanceData};
fn err(error:impl std::fmt::Display)->Error { Error(error.to_string()) }
struct Instance { raw:ash::Instance, _entry:Entry }
impl Drop for Instance { fn drop(&mut self) { unsafe { self.raw.destroy_instance(None); } } }
struct Device { raw:ash::Device, memory:vk::PhysicalDeviceMemoryProperties, _instance:Arc<Instance> }
impl Device {
    fn memory_type(&self,bits:u32,flags:vk::MemoryPropertyFlags)->Result<u32,Error> { self.memory.memory_types[..self.memory.memory_type_count as usize].iter().enumerate().find(|(i,m)| bits&(1<<i)!=0&&m.property_flags.contains(flags)).map(|(i,_)|i as u32).ok_or_else(||Error(format!("No Vulkan memory type for {flags:?}"))) }
}
impl Drop for Device { fn drop(&mut self) { unsafe { self.raw.destroy_device(None); } } }
struct Buffer { device:Arc<Device>, raw:vk::Buffer, memory:vk::DeviceMemory, mapped:*mut u8, size:usize }
impl Buffer {
    unsafe fn new(device:Arc<Device>,size:usize,usage:vk::BufferUsageFlags)->Result<Self,Error> { unsafe {
        let size=size.max(256).next_power_of_two();
        let raw=device.raw.create_buffer(&vk::BufferCreateInfo::default().size(size as u64).usage(usage).sharing_mode(vk::SharingMode::EXCLUSIVE),None).map_err(err)?;
        let requirements=device.raw.get_buffer_memory_requirements(raw);
        let memory=device.raw.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(requirements.size).memory_type_index(device.memory_type(requirements.memory_type_bits,vk::MemoryPropertyFlags::HOST_VISIBLE|vk::MemoryPropertyFlags::HOST_COHERENT)?),None).map_err(err)?;
        device.raw.bind_buffer_memory(raw,memory,0).map_err(err)?;
        let mapped=device.raw.map_memory(memory,0,vk::WHOLE_SIZE,vk::MemoryMapFlags::empty()).map_err(err)?.cast();
        Ok(Self { device,raw,memory,mapped,size })
    } }
    unsafe fn write(device:&Arc<Device>,buffer:&mut Option<Self>,data:&[u8],usage:vk::BufferUsageFlags)->Result<(),Error> { unsafe {
        if buffer.as_ref().is_none_or(|b|b.size<data.len().max(1)) { *buffer=Some(Self::new(device.clone(),data.len(),usage)?); }
        std::ptr::copy_nonoverlapping(data.as_ptr(),buffer.as_ref().unwrap().mapped,data.len()); Ok(())
    } }
}
impl Drop for Buffer { fn drop(&mut self) { unsafe { self.device.raw.unmap_memory(self.memory); self.device.raw.destroy_buffer(self.raw,None); self.device.raw.free_memory(self.memory,None); } } }
struct Atlas { device:Arc<Device>,image:vk::Image,memory:vk::DeviceMemory,view:vk::ImageView,set:vk::DescriptorSet,initialized:bool }
impl Drop for Atlas { fn drop(&mut self) { unsafe { self.device.raw.destroy_image_view(self.view,None); self.device.raw.destroy_image(self.image,None); self.device.raw.free_memory(self.memory,None); } } }
struct Slot { command:vk::CommandBuffer,fence:vk::Fence,acquired:vk::Semaphore,uniform_set:vk::DescriptorSet,text:Option<Buffer>,rects:Option<Buffer>,uniforms:Option<Buffer>,upload:Option<Buffer> }
pub(super) struct Gpu {
    instance:Arc<Instance>,device:Arc<Device>,physical:vk::PhysicalDevice,queue:vk::Queue,
    surface_api:ash::khr::surface::Instance,surface:vk::SurfaceKHR,swap_api:ash::khr::swapchain::Device,swapchain:vk::SwapchainKHR,
    extent:vk::Extent2D,format:vk::Format,views:Vec<vk::ImageView>,framebuffers:Vec<vk::Framebuffer>,present_ready:Vec<vk::Semaphore>,
    pass:vk::RenderPass,layout:vk::PipelineLayout,bg:vk::Pipeline,text:vk::Pipeline,rects:vk::Pipeline,
    pool:vk::CommandPool,descriptors:vk::DescriptorPool,uniform_layout:vk::DescriptorSetLayout,atlas_layout:vk::DescriptorSetLayout,sampler:vk::Sampler,
    slots:Vec<Slot>,slot:usize,atlases:Vec<Atlas>,transparent:bool,
}
impl Gpu {
    pub(super) fn new(window:Rc<Window>,opacity:f32)->Result<Self,Error> { unsafe {
        let entry=Entry::load().map_err(err)?;
        let app=vk::ApplicationInfo::default().application_name(c"Alacritty").api_version(vk::API_VERSION_1_1);
        let extensions=[ash::khr::surface::NAME.as_ptr(),ash::khr::xcb_surface::NAME.as_ptr()];
        let raw=entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app).enabled_extension_names(&extensions),None).map_err(err)?;
        let surface_api=ash::khr::surface::Instance::new(&entry,&raw);
        let xcb_api=ash::khr::xcb_surface::Instance::new(&entry,&raw);
        let surface=xcb_api.create_xcb_surface(&vk::XcbSurfaceCreateInfoKHR::default().connection(window.xcb_connection().cast()).window(window.xid),None).map_err(err)?;
        let instance=Arc::new(Instance { raw,_entry:entry });
        let mut selected=None;
        for physical in instance.raw.enumerate_physical_devices().map_err(err)? {
            let families=instance.raw.get_physical_device_queue_family_properties(physical);
            if let Some((family,_))=families.iter().enumerate().find(|(i,f)|f.queue_flags.contains(vk::QueueFlags::GRAPHICS)&&surface_api.get_physical_device_surface_support(physical,*i as u32,surface).unwrap_or(false)) { selected=Some((physical,family as u32)); break; }
        }
        let (physical,family)=selected.ok_or_else(||Error("No Vulkan graphics queue with presentation support".into()))?;
        let priority=[1.]; let queue_info=[vk::DeviceQueueCreateInfo::default().queue_family_index(family).queue_priorities(&priority)]; let extensions=[ash::khr::swapchain::NAME.as_ptr()];
        let raw=instance.raw.create_device(physical,&vk::DeviceCreateInfo::default().queue_create_infos(&queue_info).enabled_extension_names(&extensions),None).map_err(err)?;
        let swap_api=ash::khr::swapchain::Device::new(&instance.raw,&raw);
        let queue=raw.get_device_queue(family,0); let memory=instance.raw.get_physical_device_memory_properties(physical);
        let device=Arc::new(Device { raw,memory,_instance:instance.clone() }); let vk=&device.raw;
        let uniform_binding=[vk::DescriptorSetLayoutBinding::default().binding(0).descriptor_type(vk::DescriptorType::UNIFORM_BUFFER).descriptor_count(1).stage_flags(vk::ShaderStageFlags::VERTEX|vk::ShaderStageFlags::FRAGMENT)];
        let uniform_layout=vk.create_descriptor_set_layout(&vk::DescriptorSetLayoutCreateInfo::default().bindings(&uniform_binding),None).map_err(err)?;
        let atlas_bindings=[vk::DescriptorSetLayoutBinding::default().binding(0).descriptor_type(vk::DescriptorType::SAMPLED_IMAGE).descriptor_count(1).stage_flags(vk::ShaderStageFlags::FRAGMENT),vk::DescriptorSetLayoutBinding::default().binding(1).descriptor_type(vk::DescriptorType::SAMPLER).descriptor_count(1).stage_flags(vk::ShaderStageFlags::FRAGMENT)];
        let atlas_layout=vk.create_descriptor_set_layout(&vk::DescriptorSetLayoutCreateInfo::default().bindings(&atlas_bindings),None).map_err(err)?;
        let layouts=[uniform_layout,atlas_layout]; let layout=vk.create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default().set_layouts(&layouts),None).map_err(err)?;
        let sizes=[vk::DescriptorPoolSize { ty:vk::DescriptorType::UNIFORM_BUFFER,descriptor_count:3 },vk::DescriptorPoolSize { ty:vk::DescriptorType::SAMPLED_IMAGE,descriptor_count:4096 },vk::DescriptorPoolSize { ty:vk::DescriptorType::SAMPLER,descriptor_count:4096 }];
        let descriptors=vk.create_descriptor_pool(&vk::DescriptorPoolCreateInfo::default().max_sets(4099).pool_sizes(&sizes),None).map_err(err)?;
        let sampler=vk.create_sampler(&vk::SamplerCreateInfo::default().mag_filter(vk::Filter::LINEAR).min_filter(vk::Filter::LINEAR).mipmap_mode(vk::SamplerMipmapMode::NEAREST).address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE).address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE).address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE).max_lod(0.),None).map_err(err)?;
        let pool=vk.create_command_pool(&vk::CommandPoolCreateInfo::default().queue_family_index(family).flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),None).map_err(err)?;
        let commands=vk.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).level(vk::CommandBufferLevel::PRIMARY).command_buffer_count(3)).map_err(err)?;
        let uniform_layouts=[uniform_layout;3]; let uniform_sets=vk.allocate_descriptor_sets(&vk::DescriptorSetAllocateInfo::default().descriptor_pool(descriptors).set_layouts(&uniform_layouts)).map_err(err)?;
        let mut slots=Vec::new(); for (command,uniform_set) in commands.into_iter().zip(uniform_sets) { slots.push(Slot { command,uniform_set,fence:vk.create_fence(&vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),None).map_err(err)?,acquired:vk.create_semaphore(&vk::SemaphoreCreateInfo::default(),None).map_err(err)?,text:None,rects:None,uniforms:None,upload:None }); }
        let mut gpu=Self { instance,device,physical,queue,surface_api,surface,swap_api,swapchain:vk::SwapchainKHR::null(),extent:vk::Extent2D::default(),format:vk::Format::UNDEFINED,views:Vec::new(),framebuffers:Vec::new(),present_ready:Vec::new(),pass:vk::RenderPass::null(),layout,bg:vk::Pipeline::null(),text:vk::Pipeline::null(),rects:vk::Pipeline::null(),pool,descriptors,uniform_layout,atlas_layout,sampler,slots,slot:0,atlases:Vec::new(),transparent:opacity<1. };
        gpu.recreate_swapchain(&window)?; Ok(gpu)
    } }
    unsafe fn recreate_swapchain(&mut self,window:&Window)->Result<(),Error> { unsafe {
        let size=window.inner_size(); if size.width==0||size.height==0 { return Ok(()); }
        self.device.raw.device_wait_idle().map_err(err)?;
        let capabilities=self.surface_api.get_physical_device_surface_capabilities(self.physical,self.surface).map_err(err)?;
        let formats=self.surface_api.get_physical_device_surface_formats(self.physical,self.surface).map_err(err)?;
        let format=formats.iter().find(|f|f.format==vk::Format::B8G8R8A8_SRGB||f.format==vk::Format::R8G8B8A8_SRGB).copied().ok_or_else(||Error("Vulkan surface lacks an sRGB format".into()))?;
        let extent=if capabilities.current_extent.width!=u32::MAX { capabilities.current_extent } else { vk::Extent2D { width:size.width.clamp(capabilities.min_image_extent.width,capabilities.max_image_extent.width),height:size.height.clamp(capabilities.min_image_extent.height,capabilities.max_image_extent.height) } };
        let mut count=capabilities.min_image_count.max(3); if capabilities.max_image_count>0 { count=count.min(capabilities.max_image_count); }
        let alpha=if self.transparent&&capabilities.supported_composite_alpha.contains(vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED) { vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED } else if capabilities.supported_composite_alpha.contains(vk::CompositeAlphaFlagsKHR::OPAQUE) { vk::CompositeAlphaFlagsKHR::OPAQUE } else { [vk::CompositeAlphaFlagsKHR::POST_MULTIPLIED,vk::CompositeAlphaFlagsKHR::INHERIT].into_iter().find(|v|capabilities.supported_composite_alpha.contains(*v)).ok_or_else(||Error("No Vulkan surface alpha mode".into()))? };
        let swapchain=self.swap_api.create_swapchain(&vk::SwapchainCreateInfoKHR::default().surface(self.surface).min_image_count(count).image_format(format.format).image_color_space(format.color_space).image_extent(extent).image_array_layers(1).image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT).image_sharing_mode(vk::SharingMode::EXCLUSIVE).pre_transform(capabilities.current_transform).composite_alpha(alpha).present_mode(vk::PresentModeKHR::FIFO).clipped(true).old_swapchain(self.swapchain),None).map_err(err)?;
        for framebuffer in self.framebuffers.drain(..) { self.device.raw.destroy_framebuffer(framebuffer,None); } for view in self.views.drain(..) { self.device.raw.destroy_image_view(view,None); } for semaphore in self.present_ready.drain(..) { self.device.raw.destroy_semaphore(semaphore,None); }
        if self.swapchain!=vk::SwapchainKHR::null() { self.swap_api.destroy_swapchain(self.swapchain,None); }
        self.swapchain=swapchain; self.extent=extent;
        if self.format!=format.format {
            for pipeline in [self.bg,self.text,self.rects] { if pipeline!=vk::Pipeline::null() { self.device.raw.destroy_pipeline(pipeline,None); } }
            if self.pass!=vk::RenderPass::null() { self.device.raw.destroy_render_pass(self.pass,None); }
            self.format=format.format; self.make_pipelines()?;
        }
        for image in self.swap_api.get_swapchain_images(self.swapchain).map_err(err)? {
            let view=self.device.raw.create_image_view(&vk::ImageViewCreateInfo::default().image(image).view_type(vk::ImageViewType::TYPE_2D).format(self.format).subresource_range(vk::ImageSubresourceRange::default().aspect_mask(vk::ImageAspectFlags::COLOR).level_count(1).layer_count(1)),None).map_err(err)?;
            self.views.push(view); let attachments=[view]; self.framebuffers.push(self.device.raw.create_framebuffer(&vk::FramebufferCreateInfo::default().render_pass(self.pass).attachments(&attachments).width(extent.width).height(extent.height).layers(1),None).map_err(err)?);
            self.present_ready.push(self.device.raw.create_semaphore(&vk::SemaphoreCreateInfo::default(),None).map_err(err)?);
        }
        Ok(())
    } }
    unsafe fn make_pipelines(&mut self)->Result<(),Error> { unsafe {
        let attachments=[vk::AttachmentDescription::default().format(self.format).samples(vk::SampleCountFlags::TYPE_1).load_op(vk::AttachmentLoadOp::CLEAR).store_op(vk::AttachmentStoreOp::STORE).initial_layout(vk::ImageLayout::UNDEFINED).final_layout(vk::ImageLayout::PRESENT_SRC_KHR)];
        let reference=[vk::AttachmentReference { attachment:0,layout:vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL }]; let subpasses=[vk::SubpassDescription::default().pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS).color_attachments(&reference)];
        let dependencies=[vk::SubpassDependency::default().src_subpass(vk::SUBPASS_EXTERNAL).dst_subpass(0).src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT).dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT).dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)];
        self.pass=self.device.raw.create_render_pass(&vk::RenderPassCreateInfo::default().attachments(&attachments).subpasses(&subpasses).dependencies(&dependencies),None).map_err(err)?;
        let attr=|location,format,offset|vk::VertexInputAttributeDescription { location,binding:0,format,offset };
        let text_attrs=[attr(0,vk::Format::R16G16_UINT,0),attr(1,vk::Format::R16G16B16A16_SINT,4),attr(2,vk::Format::R32G32B32A32_SFLOAT,12),attr(3,vk::Format::R8G8B8A8_UINT,28),attr(4,vk::Format::R8G8B8A8_UNORM,32)];
        let rect_attrs=[attr(0,vk::Format::R32G32B32A32_SFLOAT,0),attr(1,vk::Format::R32G32B32A32_SFLOAT,16),attr(2,vk::Format::R32_UINT,32)];
        let make=|vertex:&[u8],fragment:&[u8],vname:&std::ffi::CStr,fname:&std::ffi::CStr,attrs:&[vk::VertexInputAttributeDescription],premultiplied:bool|->Result<vk::Pipeline,Error> {
            let decode=|bytes:&[u8]|bytes.chunks_exact(4).map(|word|u32::from_le_bytes(word.try_into().unwrap())).collect::<Vec<_>>();
            let vs=self.device.raw.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&decode(vertex)),None).map_err(err)?;
            let fs=self.device.raw.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&decode(fragment)),None).map_err(err)?;
            let stages=[vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::VERTEX).module(vs).name(vname),vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::FRAGMENT).module(fs).name(fname)];
            let bindings=[vk::VertexInputBindingDescription { binding:0,stride:36,input_rate:vk::VertexInputRate::INSTANCE }]; let input=vk::PipelineVertexInputStateCreateInfo::default().vertex_binding_descriptions(&bindings).vertex_attribute_descriptions(attrs);
            let assembly=vk::PipelineInputAssemblyStateCreateInfo::default().topology(vk::PrimitiveTopology::TRIANGLE_LIST);
            let viewport=vk::PipelineViewportStateCreateInfo::default().viewport_count(1).scissor_count(1);
            let raster=vk::PipelineRasterizationStateCreateInfo::default().polygon_mode(vk::PolygonMode::FILL).cull_mode(vk::CullModeFlags::NONE).line_width(1.);
            let samples=vk::PipelineMultisampleStateCreateInfo::default().rasterization_samples(vk::SampleCountFlags::TYPE_1);
            let attachments=[vk::PipelineColorBlendAttachmentState::default().blend_enable(true).src_color_blend_factor(if premultiplied { vk::BlendFactor::ONE } else { vk::BlendFactor::SRC_ALPHA }).dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA).color_blend_op(vk::BlendOp::ADD).src_alpha_blend_factor(vk::BlendFactor::ONE).dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA).alpha_blend_op(vk::BlendOp::ADD).color_write_mask(vk::ColorComponentFlags::RGBA)];
            let blend=vk::PipelineColorBlendStateCreateInfo::default().attachments(&attachments); let states=[vk::DynamicState::VIEWPORT,vk::DynamicState::SCISSOR]; let dynamic=vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&states);
            let info=vk::GraphicsPipelineCreateInfo::default().stages(&stages).vertex_input_state(&input).input_assembly_state(&assembly).viewport_state(&viewport).rasterization_state(&raster).multisample_state(&samples).color_blend_state(&blend).dynamic_state(&dynamic).layout(self.layout).render_pass(self.pass);
            let result=self.device.raw.create_graphics_pipelines(vk::PipelineCache::null(),&[info],None).map_err(|(_,e)|err(e)).map(|p|p[0]);
            self.device.raw.destroy_shader_module(vs,None); self.device.raw.destroy_shader_module(fs,None); result
        };
        self.bg=make(include_bytes!(concat!(env!("OUT_DIR"),"/vs_bg.spv")),include_bytes!(concat!(env!("OUT_DIR"),"/fs_bg.spv")),c"vs_bg",c"fs_bg",&text_attrs,true)?;
        self.text=make(include_bytes!(concat!(env!("OUT_DIR"),"/vs_text.spv")),include_bytes!(concat!(env!("OUT_DIR"),"/fs_text.spv")),c"vs_text",c"fs_text",&text_attrs,false)?;
        self.rects=make(include_bytes!(concat!(env!("OUT_DIR"),"/vs_main.spv")),include_bytes!(concat!(env!("OUT_DIR"),"/fs_main.spv")),c"vs_main",c"fs_main",&rect_attrs,false)?; Ok(())
    } }
    unsafe fn create_atlas(&self,width:u32,height:u32)->Result<Atlas,Error> { unsafe {
        let device=&self.device.raw;
        let image=device.create_image(&vk::ImageCreateInfo::default().image_type(vk::ImageType::TYPE_2D).format(vk::Format::R8G8B8A8_SRGB).extent(vk::Extent3D { width,height,depth:1 }).mip_levels(1).array_layers(1).samples(vk::SampleCountFlags::TYPE_1).tiling(vk::ImageTiling::OPTIMAL).usage(vk::ImageUsageFlags::SAMPLED|vk::ImageUsageFlags::TRANSFER_DST).sharing_mode(vk::SharingMode::EXCLUSIVE),None).map_err(err)?;
        let requirements=device.get_image_memory_requirements(image); let memory=device.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(requirements.size).memory_type_index(self.device.memory_type(requirements.memory_type_bits,vk::MemoryPropertyFlags::DEVICE_LOCAL)?),None).map_err(err)?; device.bind_image_memory(image,memory,0).map_err(err)?;
        let view=device.create_image_view(&vk::ImageViewCreateInfo::default().image(image).view_type(vk::ImageViewType::TYPE_2D).format(vk::Format::R8G8B8A8_SRGB).subresource_range(vk::ImageSubresourceRange::default().aspect_mask(vk::ImageAspectFlags::COLOR).level_count(1).layer_count(1)),None).map_err(err)?;
        let layouts=[self.atlas_layout]; let set=device.allocate_descriptor_sets(&vk::DescriptorSetAllocateInfo::default().descriptor_pool(self.descriptors).set_layouts(&layouts)).map_err(err)?[0];
        let image_info=[vk::DescriptorImageInfo::default().image_view(view).image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)]; let sampler_info=[vk::DescriptorImageInfo::default().sampler(self.sampler)];
        device.update_descriptor_sets(&[vk::WriteDescriptorSet::default().dst_set(set).dst_binding(0).descriptor_type(vk::DescriptorType::SAMPLED_IMAGE).image_info(&image_info),vk::WriteDescriptorSet::default().dst_set(set).dst_binding(1).descriptor_type(vk::DescriptorType::SAMPLER).image_info(&sampler_info)],&[]);
        Ok(Atlas { device:self.device.clone(),image,memory,view,set,initialized:false })
    } }
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render(&mut self,window:&Window,uniforms:&FrameUniforms,viewport:[f64;4],text:&[TextInstanceData],commands:&[Vec<TextDrawCommand>;2],rects:&[RectInstanceData],atlas:&mut GlyphAtlas,clear:[f64;4])->Result<(),Error> { unsafe {
        let size=window.inner_size(); if size.width==0||size.height==0 { return Ok(()); }
        if self.extent.width!=size.width||self.extent.height!=size.height { self.recreate_swapchain(window)?; }
        let slot_index=self.slot; let fence=self.slots[slot_index].fence; self.device.raw.wait_for_fences(&[fence],true,u64::MAX).map_err(err)?;
        while self.atlases.len()<atlas.pages.len() { let page=&mut atlas.pages[self.atlases.len()]; self.atlases.push(self.create_atlas(page.width,page.height)?); page.dirty=Some(0..page.height); }
        let device=&self.device.raw; let slot=&mut self.slots[slot_index];
        Buffer::write(&self.device,&mut slot.text,bytemuck::cast_slice(text),vk::BufferUsageFlags::VERTEX_BUFFER)?;
        Buffer::write(&self.device,&mut slot.rects,bytemuck::cast_slice(rects),vk::BufferUsageFlags::VERTEX_BUFFER)?;
        Buffer::write(&self.device,&mut slot.uniforms,bytemuck::bytes_of(uniforms),vk::BufferUsageFlags::UNIFORM_BUFFER)?;
        let buffer_info=[vk::DescriptorBufferInfo { buffer:slot.uniforms.as_ref().unwrap().raw,offset:0,range:std::mem::size_of::<FrameUniforms>() as u64 }]; device.update_descriptor_sets(&[vk::WriteDescriptorSet::default().dst_set(slot.uniform_set).dst_binding(0).descriptor_type(vk::DescriptorType::UNIFORM_BUFFER).buffer_info(&buffer_info)],&[]);
        let upload_size:usize=atlas.pages.iter().filter_map(|p|p.dirty.as_ref().map(|r|(r.end-r.start) as usize*p.width as usize*4)).sum();
        if upload_size>0&&slot.upload.as_ref().is_none_or(|b|b.size<upload_size) { slot.upload=Some(Buffer::new(self.device.clone(),upload_size,vk::BufferUsageFlags::TRANSFER_SRC)?); }
        let (image_index,suboptimal)=match self.swap_api.acquire_next_image(self.swapchain,u64::MAX,slot.acquired,vk::Fence::null()) { Ok(result)=>result,Err(vk::Result::ERROR_OUT_OF_DATE_KHR)=>{ self.recreate_swapchain(window)?; window.request_redraw(); return Ok(()); },Err(error)=>return Err(err(error)) };
        device.reset_command_buffer(slot.command,vk::CommandBufferResetFlags::empty()).map_err(err)?; device.begin_command_buffer(slot.command,&vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT)).map_err(err)?;
        if upload_size>0 {
            let upload=slot.upload.as_ref().unwrap(); let mut offset=0;
            for (page,texture) in atlas.pages.iter_mut().zip(&mut self.atlases) {
                let Some(rows)=page.dirty.take() else { continue }; let pitch=page.width as usize*4; let len=(rows.end-rows.start) as usize*pitch;
                std::ptr::copy_nonoverlapping(page.pixels.as_ptr().add(rows.start as usize*pitch),upload.mapped.add(offset),len);
                let range=vk::ImageSubresourceRange::default().aspect_mask(vk::ImageAspectFlags::COLOR).level_count(1).layer_count(1);
                let barrier=vk::ImageMemoryBarrier::default().image(texture.image).subresource_range(range).old_layout(if texture.initialized { vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL } else { vk::ImageLayout::UNDEFINED }).new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL).src_access_mask(if texture.initialized { vk::AccessFlags::SHADER_READ } else { vk::AccessFlags::empty() }).dst_access_mask(vk::AccessFlags::TRANSFER_WRITE).src_queue_family_index(vk::QUEUE_FAMILY_IGNORED).dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED);
                device.cmd_pipeline_barrier(slot.command,if texture.initialized { vk::PipelineStageFlags::FRAGMENT_SHADER } else { vk::PipelineStageFlags::TOP_OF_PIPE },vk::PipelineStageFlags::TRANSFER,vk::DependencyFlags::empty(),&[],&[],&[barrier]);
                let region=vk::BufferImageCopy::default().buffer_offset(offset as u64).image_subresource(vk::ImageSubresourceLayers::default().aspect_mask(vk::ImageAspectFlags::COLOR).layer_count(1)).image_offset(vk::Offset3D { x:0,y:rows.start as i32,z:0 }).image_extent(vk::Extent3D { width:page.width,height:rows.end-rows.start,depth:1 });
                device.cmd_copy_buffer_to_image(slot.command,upload.raw,texture.image,vk::ImageLayout::TRANSFER_DST_OPTIMAL,&[region]);
                let barrier=barrier.old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL).new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL).src_access_mask(vk::AccessFlags::TRANSFER_WRITE).dst_access_mask(vk::AccessFlags::SHADER_READ);
                device.cmd_pipeline_barrier(slot.command,vk::PipelineStageFlags::TRANSFER,vk::PipelineStageFlags::FRAGMENT_SHADER,vk::DependencyFlags::empty(),&[],&[],&[barrier]); texture.initialized=true; offset+=len;
            }
        }
        let clears=[vk::ClearValue { color:vk::ClearColorValue { float32:clear.map(|v|v as f32) } }]; let area=vk::Rect2D { offset:vk::Offset2D::default(),extent:self.extent };
        device.cmd_begin_render_pass(slot.command,&vk::RenderPassBeginInfo::default().render_pass(self.pass).framebuffer(self.framebuffers[image_index as usize]).render_area(area).clear_values(&clears),vk::SubpassContents::INLINE);
        device.cmd_set_viewport(slot.command,0,&[vk::Viewport { x:viewport[0] as f32,y:viewport[1] as f32,width:viewport[2] as f32,height:viewport[3] as f32,min_depth:0.,max_depth:1. }]);
        device.cmd_set_scissor(slot.command,0,&[vk::Rect2D { offset:vk::Offset2D { x:viewport[0] as i32,y:viewport[1] as i32 },extent:vk::Extent2D { width:viewport[2] as u32,height:viewport[3] as u32 } }]);
        device.cmd_bind_descriptor_sets(slot.command,vk::PipelineBindPoint::GRAPHICS,self.layout,0,&[slot.uniform_set],&[]);
        let draw_layer=|layer:&[TextDrawCommand]| {
            device.cmd_bind_vertex_buffers(slot.command,0,&[slot.text.as_ref().unwrap().raw],&[0]);
            for command in layer {
                let range=match command { TextDrawCommand::Background(range)=>{ device.cmd_bind_pipeline(slot.command,vk::PipelineBindPoint::GRAPHICS,self.bg); range },TextDrawCommand::Glyph { atlas_index,instance_range }=>{ device.cmd_bind_pipeline(slot.command,vk::PipelineBindPoint::GRAPHICS,self.text); device.cmd_bind_descriptor_sets(slot.command,vk::PipelineBindPoint::GRAPHICS,self.layout,1,&[self.atlases[*atlas_index].set],&[]); instance_range } };
                device.cmd_draw(slot.command,6,range.end-range.start,0,range.start);
            }
        };
        draw_layer(&commands[0]); if !rects.is_empty() { device.cmd_bind_pipeline(slot.command,vk::PipelineBindPoint::GRAPHICS,self.rects); device.cmd_bind_vertex_buffers(slot.command,0,&[slot.rects.as_ref().unwrap().raw],&[0]); device.cmd_draw(slot.command,6,rects.len() as u32,0,0); } draw_layer(&commands[1]);
        device.cmd_end_render_pass(slot.command); device.end_command_buffer(slot.command).map_err(err)?;
        let waits=[slot.acquired]; let stages=[vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT]; let signals=[self.present_ready[image_index as usize]]; let buffers=[slot.command]; let submit=vk::SubmitInfo::default().wait_semaphores(&waits).wait_dst_stage_mask(&stages).command_buffers(&buffers).signal_semaphores(&signals);
        device.reset_fences(&[slot.fence]).map_err(err)?; device.queue_submit(self.queue,&[submit],slot.fence).map_err(err)?;
        let swaps=[self.swapchain]; let indices=[image_index]; let present=vk::PresentInfoKHR::default().wait_semaphores(&signals).swapchains(&swaps).image_indices(&indices);
        let outdated=match self.swap_api.queue_present(self.queue,&present) { Ok(changed)=>changed,Err(vk::Result::ERROR_OUT_OF_DATE_KHR)=>true,Err(error)=>return Err(err(error)) };
        self.slot=(self.slot+1)%self.slots.len(); if suboptimal||outdated { self.recreate_swapchain(window)?; } Ok(())
    } }
}
impl Drop for Gpu { fn drop(&mut self) { unsafe {
    let device=&self.device.raw; let _=device.device_wait_idle();
    self.atlases.clear(); for slot in self.slots.drain(..) { device.destroy_fence(slot.fence,None); device.destroy_semaphore(slot.acquired,None); }
    for framebuffer in self.framebuffers.drain(..) { device.destroy_framebuffer(framebuffer,None); } for view in self.views.drain(..) { device.destroy_image_view(view,None); } for semaphore in self.present_ready.drain(..) { device.destroy_semaphore(semaphore,None); }
    for pipeline in [self.bg,self.text,self.rects] { device.destroy_pipeline(pipeline,None); } device.destroy_render_pass(self.pass,None); device.destroy_pipeline_layout(self.layout,None); device.destroy_sampler(self.sampler,None); device.destroy_descriptor_pool(self.descriptors,None); device.destroy_descriptor_set_layout(self.uniform_layout,None); device.destroy_descriptor_set_layout(self.atlas_layout,None); device.destroy_command_pool(self.pool,None); self.swap_api.destroy_swapchain(self.swapchain,None); self.surface_api.destroy_surface(self.surface,None);
} } }
