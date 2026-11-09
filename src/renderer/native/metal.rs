//! Metal resources and command submission.
use std::rc::Rc;
use cidre::{arc, ca, cg, mtl, ns, objc};
use crate::{platform::window::Window, renderer::Error};
fn pool<T>(f:impl FnOnce()->T)->T { let mut value=None; objc::ar_pool(|| { value=Some(f()); }); value.unwrap() }
use super::{FrameUniforms, TextInstanceData, RectInstanceData, TextDrawCommand, atlas::GlyphAtlas};
#[derive(Default)]
struct Slot { text:Option<arc::R<mtl::Buf>>, rects:Option<arc::R<mtl::Buf>>, uniforms:Option<arc::R<mtl::Buf>>, upload:Option<arc::R<mtl::Buf>>, completion:Option<arc::R<mtl::CmdBuf>> }
pub(super) struct Gpu {
    device:arc::R<mtl::Device>, queue:arc::R<mtl::CmdQueue>, layer:arc::R<ca::MetalLayer>,
    bg:arc::R<mtl::RenderPipelineState>, text:arc::R<mtl::RenderPipelineState>, rects:arc::R<mtl::RenderPipelineState>,
    atlases:Vec<arc::R<mtl::Texture>>, slots:[Slot;3], slot:usize,
}
impl Gpu {
    pub(super) fn new(window:Rc<Window>,_opacity:f32)->Result<Self,Error> {
        pool(|| {
            let device=mtl::Device::sys_default().ok_or_else(||Error("No Metal device".into()))?;
            let queue=device.new_cmd_queue().ok_or_else(||Error("Cannot create Metal command queue".into()))?;
            let mut layer=window.metal_layer(); layer.set_device(Some(&device)); layer.set_pixel_format(mtl::PixelFormat::Bgra8UNormSrgb); layer.set_framebuffer_only(true);
            let library=device.new_lib_with_src_blocking(&ns::String::with_str(include_str!("terminal.metal")),None).map_err(|e|Error(format!("Metal shader compilation: {e:?}")))?;
            let pipeline=|vertex:&str,fragment:&str,premultiplied:bool|->Result<_,Error> {
                let vertex=library.new_fn(&ns::String::with_str(vertex)).ok_or_else(||Error("Missing Metal vertex function".into()))?;
                let fragment=library.new_fn(&ns::String::with_str(fragment)).ok_or_else(||Error("Missing Metal fragment function".into()))?;
                let desc=mtl::RenderPipelineDesc::new().with_fns(&vertex,&fragment);
                let mut color=desc.color_attaches().get(0); color.set_pixel_format(mtl::PixelFormat::Bgra8UNormSrgb); color.set_blending_enabled(true);
                color.set_src_rgb_blend_factor(if premultiplied { mtl::BlendFactor::One } else { mtl::BlendFactor::SrcAlpha });
                color.set_dst_rgb_blend_factor(mtl::BlendFactor::OneMinusSrcAlpha); color.set_src_alpha_blend_factor(mtl::BlendFactor::One); color.set_dst_alpha_blend_factor(mtl::BlendFactor::OneMinusSrcAlpha);
                device.new_render_ps(&desc).map_err(|e|Error(format!("Metal pipeline: {e:?}")))
            };
            let bg=pipeline("vs_bg","fs_bg",true)?; let text=pipeline("vs_text","fs_text",false)?; let rects=pipeline("vs_rect","fs_rect",false)?;
            Ok(Self { device,queue,layer,bg,text,rects,atlases:Vec::new(),slots:Default::default(),slot:0 })
        })
    }
    fn buffer(device:&mtl::Device,buffer:&mut Option<arc::R<mtl::Buf>>,size:usize)->Result<(),Error> {
        if buffer.as_ref().is_none_or(|b|b.len()<size.max(1)) { *buffer=Some(device.new_buf(size.max(256).next_power_of_two(),mtl::ResOpts::STORAGE_MODE_SHARED|mtl::ResOpts::CPU_CACHE_MODE_WRITE_COMBINED).ok_or_else(||Error("Metal buffer allocation failed".into()))?); }
        Ok(())
    }
    fn write(device:&mtl::Device,buffer:&mut Option<arc::R<mtl::Buf>>,data:&[u8])->Result<(),Error> {
        Self::buffer(device,buffer,data.len())?;
        // The slot fence completed before any CPU writes to its shared buffers.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(),buffer.as_ref().unwrap().contents(),data.len()); }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render(&mut self,window:&Window,uniforms:&FrameUniforms,viewport:[f64;4],text:&[TextInstanceData],commands:&[Vec<TextDrawCommand>;2],rects:&[RectInstanceData],atlas:&mut GlyphAtlas,clear:[f64;4])->Result<(),Error> {
        pool(|| {
            let size=window.inner_size(); if size.width==0||size.height==0 { return Ok(()); }
            self.layer.set_drawable_size(cg::Size::new(size.width as f64,size.height as f64));
            let Some(drawable)=self.layer.next_drawable().map_err(|e|Error(format!("Metal drawable: {e:?}")))? else { return Ok(()) };
            let slot=&mut self.slots[self.slot]; if let Some(completion)=slot.completion.take() { completion.wait_until_completed(); }
            Self::write(&self.device,&mut slot.text,bytemuck::cast_slice(text))?;
            Self::write(&self.device,&mut slot.rects,bytemuck::cast_slice(rects))?;
            Self::write(&self.device,&mut slot.uniforms,bytemuck::bytes_of(uniforms))?;
            let mut command=self.queue.new_cmd_buf().ok_or_else(||Error("Metal command buffer allocation failed".into()))?;
            self.atlases.truncate(atlas.pages.len());
            for page in &mut atlas.pages[self.atlases.len()..] {
                let mut desc=mtl::TextureDesc::new_2d(mtl::PixelFormat::Rgba8UNormSrgb,page.width as usize,page.height as usize,false);
                desc.set_storage_mode(mtl::StorageMode::Private); desc.set_usage(mtl::TextureUsage::SHADER_READ);
                self.atlases.push(self.device.new_texture(&desc).ok_or_else(||Error("Metal atlas allocation failed".into()))?);
                page.dirty=Some(0..page.height);
            }
            let upload_size:usize=atlas.pages.iter().filter_map(|p|p.dirty.as_ref().map(|r|(r.end-r.start) as usize*p.width as usize*4)).sum();
            if upload_size>0 {
                Self::buffer(&self.device,&mut slot.upload,upload_size)?;
                let upload=slot.upload.as_ref().unwrap(); let mut offset=0;
                command.blit(|blit| {
                    for (page,texture) in atlas.pages.iter_mut().zip(self.atlases.iter_mut()) {
                        let Some(rows)=page.dirty.take() else { continue; };
                        let pitch=page.width as usize*4; let len=(rows.end-rows.start) as usize*pitch;
                        unsafe { std::ptr::copy_nonoverlapping(page.pixels.as_ptr().add(rows.start as usize*pitch),upload.contents().add(offset),len); }
                        blit.copy_from_buf_to_texture(upload,offset,pitch,len,mtl::Size { width:page.width as usize,height:(rows.end-rows.start) as usize,depth:1 },texture,0,0,mtl::Origin { x:0,y:rows.start as usize,z:0 });
                        offset+=len;
                    }
                });
            }
            let pass=mtl::RenderPassDesc::new(); let mut color=pass.color_attaches().get(0);
            color.set_texture(Some(&drawable.texture())); color.set_load_action(mtl::LoadAction::Clear); color.set_store_action(mtl::StoreAction::Store);
            color.set_clear_color(mtl::ClearColor { r:clear[0],g:clear[1],b:clear[2],a:clear[3] });
            command.render(&pass,|encoder| {
                encoder.set_vp_rect(viewport[0],viewport[1],viewport[2],viewport[3]);
                encoder.set_vertex_buf_at(slot.uniforms.as_deref(),0,1); encoder.set_fragment_buf_at(slot.uniforms.as_deref(),0,1);
                let draw_layer=|encoder:&mut mtl::RenderCmdEncoder,layer:&[TextDrawCommand]| {
                    for draw in layer {
                        let range=match draw {
                            TextDrawCommand::Background(range)=>{ encoder.set_render_ps(&self.bg); range },
                            TextDrawCommand::Glyph { atlas_index,instance_range }=>{ encoder.set_render_ps(&self.text); encoder.set_fragment_texture_at(Some(&self.atlases[*atlas_index]),0); instance_range },
                        };
                        encoder.set_vertex_buf_at(slot.text.as_deref(),range.start as usize*std::mem::size_of::<TextInstanceData>(),0);
                        encoder.draw_primitives_instance_count(mtl::Primitive::Triangle,0,6,(range.end-range.start) as usize);
                    }
                };
                draw_layer(encoder,&commands[0]);
                if !rects.is_empty() { encoder.set_render_ps(&self.rects); encoder.set_vertex_buf_at(slot.rects.as_deref(),0,0); encoder.draw_primitives_instance_count(mtl::Primitive::Triangle,0,6,rects.len()); }
                draw_layer(encoder,&commands[1]);
            });
            command.present_drawable(drawable.as_ref()); command.commit(); slot.completion=Some(command); self.slot=(self.slot+1)%self.slots.len(); Ok(())
        })
    }
}
impl Drop for Gpu { fn drop(&mut self) { for slot in &self.slots { if let Some(command)=&slot.completion { command.wait_until_completed(); } } } }
