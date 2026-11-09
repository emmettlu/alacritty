//! Direct3D 12 rendering with persistent upload slots and fence retirement.
use std::{ffi::CString,rc::Rc};
use windows::{core::{Interface,PCSTR},Win32::{Foundation::*,Graphics::{Direct3D::*,Direct3D::Fxc::*,Direct3D12::*,Dxgi::*,Dxgi::Common::*,DirectComposition::*},System::Threading::*}};
use crate::{platform::window::Window,renderer::Error};
use super::{FrameUniforms,TextInstanceData,TextDrawCommand,RectInstanceData,atlas::GlyphAtlas};
fn err(error:windows::core::Error)->Error { Error(error.to_string()) }
struct Buffer { resource:ID3D12Resource,ptr:*mut u8,size:usize }
impl Buffer {
    unsafe fn new(device:&ID3D12Device,size:usize)->Result<Self,Error> { unsafe {
        let size=size.max(256).next_power_of_two(); let heap=D3D12_HEAP_PROPERTIES { Type:D3D12_HEAP_TYPE_UPLOAD,..Default::default() };
        let desc=D3D12_RESOURCE_DESC { Dimension:D3D12_RESOURCE_DIMENSION_BUFFER,Width:size as u64,Height:1,DepthOrArraySize:1,MipLevels:1,SampleDesc:DXGI_SAMPLE_DESC { Count:1,Quality:0 },Layout:D3D12_TEXTURE_LAYOUT_ROW_MAJOR,..Default::default() };
        let mut resource=None; device.CreateCommittedResource(&heap,D3D12_HEAP_FLAG_NONE,&desc,D3D12_RESOURCE_STATE_GENERIC_READ,None,&mut resource).map_err(err)?; let resource:ID3D12Resource=resource.unwrap(); let mut ptr=std::ptr::null_mut(); resource.Map(0,Some(&D3D12_RANGE { Begin:0,End:0 }),Some(&mut ptr)).map_err(err)?; Ok(Self { resource,ptr:ptr.cast(),size })
    } }
    unsafe fn write(device:&ID3D12Device,buffer:&mut Option<Self>,data:&[u8])->Result<(),Error> { unsafe { if buffer.as_ref().is_none_or(|b|b.size<data.len().max(1)) { *buffer=Some(Self::new(device,data.len())?); } std::ptr::copy_nonoverlapping(data.as_ptr(),buffer.as_ref().unwrap().ptr,data.len()); Ok(()) } }
}
impl Drop for Buffer { fn drop(&mut self) { unsafe { self.resource.Unmap(0,None); } } }
struct Atlas { texture:ID3D12Resource,initialized:bool }
struct Slot { allocator:ID3D12CommandAllocator,text:Option<Buffer>,rects:Option<Buffer>,uniforms:Option<Buffer>,upload:Option<Buffer>,fence:u64 }
struct Composition { _device:IDCompositionDevice,_target:IDCompositionTarget,_visual:IDCompositionVisual }
pub(super) struct Gpu {
    device:ID3D12Device,queue:ID3D12CommandQueue,swapchain:IDXGISwapChain3,rtv:ID3D12DescriptorHeap,srv:ID3D12DescriptorHeap,rtv_stride:usize,srv_stride:usize,targets:Vec<ID3D12Resource>,
    root:ID3D12RootSignature,bg:ID3D12PipelineState,text:ID3D12PipelineState,rects:ID3D12PipelineState,list:ID3D12GraphicsCommandList,slots:Vec<Slot>,fence:ID3D12Fence,event:HANDLE,next_fence:u64,slot:usize,size:(u32,u32),atlases:Vec<Atlas>,_composition:Option<Composition>,
}
unsafe fn shader(entry:&str,profile:&str)->Result<ID3DBlob,Error> { unsafe {
    let source=include_bytes!("terminal.hlsl"); let entry=CString::new(entry).unwrap(); let profile=CString::new(profile).unwrap(); let mut code=None; let mut errors=None;
    D3DCompile(source.as_ptr().cast(),source.len(),PCSTR::null(),None,None,PCSTR(entry.as_ptr().cast()),PCSTR(profile.as_ptr().cast()),D3DCOMPILE_OPTIMIZATION_LEVEL3,0,&mut code,Some(&mut errors)).map_err(|error| { if let Some(errors)=errors { Error(String::from_utf8_lossy(std::slice::from_raw_parts(errors.GetBufferPointer().cast(),errors.GetBufferSize())).into_owned()) } else { err(error) } })?;
    Ok(code.unwrap())
} }
unsafe fn barrier(list:&ID3D12GraphicsCommandList,resource:&ID3D12Resource,before:D3D12_RESOURCE_STATES,after:D3D12_RESOURCE_STATES) { unsafe {
    let mut barrier=D3D12_RESOURCE_BARRIER { Type:D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,Flags:D3D12_RESOURCE_BARRIER_FLAG_NONE,Anonymous:D3D12_RESOURCE_BARRIER_0 { Transition:std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER { pResource:std::mem::ManuallyDrop::new(Some(resource.clone())),Subresource:D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,StateBefore:before,StateAfter:after }) } };
    list.ResourceBarrier(std::slice::from_ref(&barrier)); std::mem::ManuallyDrop::drop(&mut barrier.Anonymous.Transition.pResource);
} }
impl Gpu {
    pub(super) fn new(window:Rc<Window>,opacity:f32)->Result<Self,Error> { unsafe {
        let factory:IDXGIFactory4=CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).map_err(err)?;
        let mut device=None; D3D12CreateDevice(None,D3D_FEATURE_LEVEL_11_0,&mut device).map_err(err)?; let device:ID3D12Device=device.unwrap();
        let queue:ID3D12CommandQueue=device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type:D3D12_COMMAND_LIST_TYPE_DIRECT,..Default::default() }).map_err(err)?;
        let size=window.inner_size(); let transparent=opacity<1.;
        let desc=DXGI_SWAP_CHAIN_DESC1 { Width:size.width.max(1),Height:size.height.max(1),Format:DXGI_FORMAT_B8G8R8A8_UNORM,SampleDesc:DXGI_SAMPLE_DESC { Count:1,Quality:0 },BufferUsage:DXGI_USAGE_RENDER_TARGET_OUTPUT,BufferCount:3,Scaling:DXGI_SCALING_STRETCH,SwapEffect:DXGI_SWAP_EFFECT_FLIP_DISCARD,AlphaMode:if transparent { DXGI_ALPHA_MODE_PREMULTIPLIED } else { DXGI_ALPHA_MODE_IGNORE },..Default::default() };
        let swapchain=if transparent { factory.CreateSwapChainForComposition(&queue,&desc,None).map_err(err)? } else { factory.CreateSwapChainForHwnd(&queue,window.hwnd(),&desc,None,None).map_err(err)? };
        let swapchain:IDXGISwapChain3=swapchain.cast().map_err(err)?;
        let composition=if transparent { let composition:IDCompositionDevice=DCompositionCreateDevice(None).map_err(err)?; let target=composition.CreateTargetForHwnd(window.hwnd(),true).map_err(err)?; let visual=composition.CreateVisual().map_err(err)?; visual.SetContent(&swapchain).map_err(err)?; target.SetRoot(&visual).map_err(err)?; composition.Commit().map_err(err)?; Some(Composition { _device:composition,_target:target,_visual:visual }) } else { None };
        let _=factory.MakeWindowAssociation(window.hwnd(),DXGI_MWA_NO_ALT_ENTER);
        let rtv:ID3D12DescriptorHeap=device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type:D3D12_DESCRIPTOR_HEAP_TYPE_RTV,NumDescriptors:3,..Default::default() }).map_err(err)?;
        let srv:ID3D12DescriptorHeap=device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type:D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,NumDescriptors:4096,Flags:D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,..Default::default() }).map_err(err)?;
        let range=D3D12_DESCRIPTOR_RANGE { RangeType:D3D12_DESCRIPTOR_RANGE_TYPE_SRV,NumDescriptors:1,BaseShaderRegister:0,RegisterSpace:0,OffsetInDescriptorsFromTableStart:0 };
        let params=[D3D12_ROOT_PARAMETER { ParameterType:D3D12_ROOT_PARAMETER_TYPE_CBV,Anonymous:D3D12_ROOT_PARAMETER_0 { Descriptor:D3D12_ROOT_DESCRIPTOR { ShaderRegister:0,RegisterSpace:0 } },ShaderVisibility:D3D12_SHADER_VISIBILITY_ALL },D3D12_ROOT_PARAMETER { ParameterType:D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,Anonymous:D3D12_ROOT_PARAMETER_0 { DescriptorTable:D3D12_ROOT_DESCRIPTOR_TABLE { NumDescriptorRanges:1,pDescriptorRanges:&range } },ShaderVisibility:D3D12_SHADER_VISIBILITY_PIXEL }];
        let sampler=D3D12_STATIC_SAMPLER_DESC { Filter:D3D12_FILTER_MIN_MAG_MIP_POINT,AddressU:D3D12_TEXTURE_ADDRESS_MODE_CLAMP,AddressV:D3D12_TEXTURE_ADDRESS_MODE_CLAMP,AddressW:D3D12_TEXTURE_ADDRESS_MODE_CLAMP,MaxLOD:f32::MAX,ComparisonFunc:D3D12_COMPARISON_FUNC_ALWAYS,ShaderVisibility:D3D12_SHADER_VISIBILITY_PIXEL,..Default::default() };
        let desc=D3D12_ROOT_SIGNATURE_DESC { NumParameters:params.len() as u32,pParameters:params.as_ptr(),NumStaticSamplers:1,pStaticSamplers:&sampler,Flags:D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT };
        let mut signature=None; D3D12SerializeRootSignature(&desc,D3D_ROOT_SIGNATURE_VERSION_1,&mut signature,None).map_err(err)?; let signature=signature.unwrap(); let bytes=std::slice::from_raw_parts(signature.GetBufferPointer().cast(),signature.GetBufferSize()); let root:ID3D12RootSignature=device.CreateRootSignature(0,bytes).map_err(err)?;
        let layout=|name:&'static [u8],format:DXGI_FORMAT,offset|D3D12_INPUT_ELEMENT_DESC { SemanticName:PCSTR(name.as_ptr()),Format:format,InputSlot:0,AlignedByteOffset:offset,InputSlotClass:D3D12_INPUT_CLASSIFICATION_PER_INSTANCE_DATA,InstanceDataStepRate:1,..Default::default() };
        let text_layout=[layout(b"GRID\0",DXGI_FORMAT_R16G16_UINT,0),layout(b"GLYPH\0",DXGI_FORMAT_R16G16B16A16_SINT,4),layout(b"UV\0",DXGI_FORMAT_R32G32B32A32_FLOAT,12),layout(b"FG\0",DXGI_FORMAT_R8G8B8A8_UINT,28),layout(b"BG\0",DXGI_FORMAT_R8G8B8A8_UNORM,32)];
        let rect_layout=[layout(b"RECT\0",DXGI_FORMAT_R32G32B32A32_FLOAT,0),layout(b"COLOR\0",DXGI_FORMAT_R32G32B32A32_FLOAT,16),layout(b"KIND\0",DXGI_FORMAT_R32_UINT,32)];
        let pipeline=|vertex:&str,fragment:&str,layout:&[D3D12_INPUT_ELEMENT_DESC],premultiplied:bool|->Result<ID3D12PipelineState,Error> {
            let vs=shader(vertex,"vs_5_1")?; let ps=shader(fragment,"ps_5_1")?;
            let mut blend=D3D12_BLEND_DESC::default(); blend.RenderTarget[0]=D3D12_RENDER_TARGET_BLEND_DESC { BlendEnable:true.into(),SrcBlend:if premultiplied { D3D12_BLEND_ONE } else { D3D12_BLEND_SRC_ALPHA },DestBlend:D3D12_BLEND_INV_SRC_ALPHA,BlendOp:D3D12_BLEND_OP_ADD,SrcBlendAlpha:D3D12_BLEND_ONE,DestBlendAlpha:D3D12_BLEND_INV_SRC_ALPHA,BlendOpAlpha:D3D12_BLEND_OP_ADD,RenderTargetWriteMask:D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,..Default::default() };
            let mut formats=[DXGI_FORMAT_UNKNOWN;8]; formats[0]=DXGI_FORMAT_B8G8R8A8_UNORM_SRGB;
            let desc=D3D12_GRAPHICS_PIPELINE_STATE_DESC { pRootSignature:std::mem::ManuallyDrop::new(Some(root.clone())),VS:D3D12_SHADER_BYTECODE { pShaderBytecode:vs.GetBufferPointer(),BytecodeLength:vs.GetBufferSize() },PS:D3D12_SHADER_BYTECODE { pShaderBytecode:ps.GetBufferPointer(),BytecodeLength:ps.GetBufferSize() },BlendState:blend,SampleMask:u32::MAX,RasterizerState:D3D12_RASTERIZER_DESC { FillMode:D3D12_FILL_MODE_SOLID,CullMode:D3D12_CULL_MODE_NONE,DepthClipEnable:true.into(),..Default::default() },InputLayout:D3D12_INPUT_LAYOUT_DESC { pInputElementDescs:layout.as_ptr(),NumElements:layout.len() as u32 },PrimitiveTopologyType:D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,NumRenderTargets:1,RTVFormats:formats,SampleDesc:DXGI_SAMPLE_DESC { Count:1,Quality:0 },..Default::default() };
            let result=device.CreateGraphicsPipelineState(&desc).map_err(err); let mut desc=desc; std::mem::ManuallyDrop::drop(&mut desc.pRootSignature); result
        };
        let bg=pipeline("vs_bg","fs_bg",&text_layout,true)?; let text=pipeline("vs_text","fs_text",&text_layout,false)?; let rects=pipeline("vs_rect","fs_rect",&rect_layout,false)?;
        let mut slots=Vec::new(); for _ in 0..3 { slots.push(Slot { allocator:device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).map_err(err)?,text:None,rects:None,uniforms:None,upload:None,fence:0 }); }
        let list:ID3D12GraphicsCommandList=device.CreateCommandList(0,D3D12_COMMAND_LIST_TYPE_DIRECT,&slots[0].allocator,None).map_err(err)?; list.Close().map_err(err)?;
        let fence:ID3D12Fence=device.CreateFence(0,D3D12_FENCE_FLAG_NONE).map_err(err)?; let event=CreateEventW(None,false,false,None).map_err(err)?;
        let mut gpu=Self { rtv_stride:device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) as usize,srv_stride:device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize,device,queue,swapchain,rtv,srv,targets:Vec::new(),root,bg,text,rects,list,slots,fence,event,next_fence:1,slot:0,size:(size.width,size.height),atlases:Vec::new(),_composition:composition }; gpu.targets()?; Ok(gpu)
    } }
    unsafe fn targets(&mut self)->Result<(),Error> { unsafe { for i in 0..3 { let resource:ID3D12Resource=self.swapchain.GetBuffer(i).map_err(err)?; let handle=D3D12_CPU_DESCRIPTOR_HANDLE { ptr:self.rtv.GetCPUDescriptorHandleForHeapStart().ptr+i as usize*self.rtv_stride }; self.device.CreateRenderTargetView(&resource,Some(&D3D12_RENDER_TARGET_VIEW_DESC { Format:DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,ViewDimension:D3D12_RTV_DIMENSION_TEXTURE2D,..Default::default() }),handle); self.targets.push(resource); } Ok(()) } }
    unsafe fn wait_fence(&self,value:u64)->Result<(),Error> { unsafe { if self.fence.GetCompletedValue()<value { self.fence.SetEventOnCompletion(value,self.event).map_err(err)?; WaitForSingleObject(self.event,u32::MAX); } Ok(()) } }
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render(&mut self,window:&Window,uniforms:&FrameUniforms,viewport:[f64;4],text:&[TextInstanceData],commands:&[Vec<TextDrawCommand>;2],rects:&[RectInstanceData],atlas:&mut GlyphAtlas,clear:[f64;4])->Result<(),Error> { unsafe {
        let size=window.inner_size(); if size.width==0||size.height==0 { return Ok(()); }
        if self.size!=(size.width,size.height) { for slot in &self.slots { self.wait_fence(slot.fence)?; } self.targets.clear(); self.swapchain.ResizeBuffers(3,size.width,size.height,DXGI_FORMAT_B8G8R8A8_UNORM,DXGI_SWAP_CHAIN_FLAG(0)).map_err(err)?; self.targets()?; self.size=(size.width,size.height); }
        let index=self.slot; self.wait_fence(self.slots[index].fence)?; let slot=&mut self.slots[index];
        Buffer::write(&self.device,&mut slot.text,bytemuck::cast_slice(text))?; Buffer::write(&self.device,&mut slot.rects,bytemuck::cast_slice(rects))?; Buffer::write(&self.device,&mut slot.uniforms,bytemuck::bytes_of(uniforms))?;
        slot.allocator.Reset().map_err(err)?; self.list.Reset(&slot.allocator,None).map_err(err)?;
        // Keep atlas descriptors stable while earlier frames are using the heap.
        while self.atlases.len()<atlas.pages.len() {
            let page=&mut atlas.pages[self.atlases.len()]; let heap=D3D12_HEAP_PROPERTIES { Type:D3D12_HEAP_TYPE_DEFAULT,..Default::default() };
            let desc=D3D12_RESOURCE_DESC { Dimension:D3D12_RESOURCE_DIMENSION_TEXTURE2D,Width:page.width as u64,Height:page.height,DepthOrArraySize:1,MipLevels:1,Format:DXGI_FORMAT_R8G8B8A8_UNORM,SampleDesc:DXGI_SAMPLE_DESC { Count:1,Quality:0 },..Default::default() };
            let mut resource=None; self.device.CreateCommittedResource(&heap,D3D12_HEAP_FLAG_NONE,&desc,D3D12_RESOURCE_STATE_COPY_DEST,None,&mut resource).map_err(err)?; let texture:ID3D12Resource=resource.unwrap();
            if self.atlases.len()>=4096 { return Err(Error("DX12 atlas descriptor capacity exhausted".into())); }
            let handle=D3D12_CPU_DESCRIPTOR_HANDLE { ptr:self.srv.GetCPUDescriptorHandleForHeapStart().ptr+self.atlases.len()*self.srv_stride };
            self.device.CreateShaderResourceView(&texture,Some(&D3D12_SHADER_RESOURCE_VIEW_DESC { Format:DXGI_FORMAT_R8G8B8A8_UNORM_SRGB,ViewDimension:D3D12_SRV_DIMENSION_TEXTURE2D,Shader4ComponentMapping:D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,Anonymous:D3D12_SHADER_RESOURCE_VIEW_DESC_0 { Texture2D:D3D12_TEX2D_SRV { MipLevels:1,..Default::default() } } }),handle);
            self.atlases.push(Atlas { texture,initialized:false }); page.dirty=Some(0..page.height);
        }
        let upload_size:usize=atlas.pages.iter().filter_map(|page|page.dirty.as_ref().map(|r|((page.width as usize*4+255)&!255)*(r.end-r.start) as usize)).map(|n|(n+511)&!511).sum();
        if upload_size>0 {
            if slot.upload.as_ref().is_none_or(|b|b.size<upload_size) { slot.upload=Some(Buffer::new(&self.device,upload_size)?); }
            let upload=slot.upload.as_ref().unwrap(); let mut offset=0;
            for (page,texture) in atlas.pages.iter_mut().zip(&mut self.atlases) {
                let Some(rows)=page.dirty.take() else { continue }; let row_size=page.width as usize*4; let pitch=(row_size+255)&!255; let height=rows.end-rows.start;
                for row in 0..height as usize { std::ptr::copy_nonoverlapping(page.pixels.as_ptr().add((rows.start as usize+row)*row_size),upload.ptr.add(offset+row*pitch),row_size); }
                if texture.initialized { barrier(&self.list,&texture.texture,D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,D3D12_RESOURCE_STATE_COPY_DEST); }
                let src=D3D12_TEXTURE_COPY_LOCATION { pResource:std::mem::ManuallyDrop::new(Some(upload.resource.clone())),Type:D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,Anonymous:D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint:D3D12_PLACED_SUBRESOURCE_FOOTPRINT { Offset:offset as u64,Footprint:D3D12_SUBRESOURCE_FOOTPRINT { Format:DXGI_FORMAT_R8G8B8A8_UNORM,Width:page.width,Height:height,Depth:1,RowPitch:pitch as u32 } } } };
                let dst=D3D12_TEXTURE_COPY_LOCATION { pResource:std::mem::ManuallyDrop::new(Some(texture.texture.clone())),Type:D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,Anonymous:D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex:0 } };
                self.list.CopyTextureRegion(&dst,0,rows.start,0,&src,None); let mut src=src; let mut dst=dst; std::mem::ManuallyDrop::drop(&mut src.pResource); std::mem::ManuallyDrop::drop(&mut dst.pResource);
                barrier(&self.list,&texture.texture,D3D12_RESOURCE_STATE_COPY_DEST,D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE); texture.initialized=true; offset=(offset+height as usize*pitch+511)&!511;
            }
        }
        let target=self.swapchain.GetCurrentBackBufferIndex() as usize; let handle=D3D12_CPU_DESCRIPTOR_HANDLE { ptr:self.rtv.GetCPUDescriptorHandleForHeapStart().ptr+target*self.rtv_stride };
        barrier(&self.list,&self.targets[target],D3D12_RESOURCE_STATE_PRESENT,D3D12_RESOURCE_STATE_RENDER_TARGET);
        self.list.ClearRenderTargetView(handle,&clear.map(|v|v as f32),None); self.list.OMSetRenderTargets(1,Some(&handle),false,None);
        self.list.RSSetViewports(&[D3D12_VIEWPORT { TopLeftX:viewport[0] as f32,TopLeftY:viewport[1] as f32,Width:viewport[2] as f32,Height:viewport[3] as f32,MinDepth:0.,MaxDepth:1. }]);
        self.list.RSSetScissorRects(&[RECT { left:viewport[0] as i32,top:viewport[1] as i32,right:(viewport[0]+viewport[2]) as i32,bottom:(viewport[1]+viewport[3]) as i32 }]);
        self.list.SetGraphicsRootSignature(&self.root); self.list.SetDescriptorHeaps(&[Some(self.srv.clone())]); self.list.SetGraphicsRootConstantBufferView(0,slot.uniforms.as_ref().unwrap().resource.GetGPUVirtualAddress()); self.list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        let draw_layer=|layer:&[TextDrawCommand]| {
            for command in layer { let range=match command { TextDrawCommand::Background(range)=>{ self.list.SetPipelineState(&self.bg); range },TextDrawCommand::Glyph { atlas_index,instance_range }=>{ self.list.SetPipelineState(&self.text); self.list.SetGraphicsRootDescriptorTable(1,D3D12_GPU_DESCRIPTOR_HANDLE { ptr:self.srv.GetGPUDescriptorHandleForHeapStart().ptr+(*atlas_index*self.srv_stride) as u64 }); instance_range } };
                let buffer=slot.text.as_ref().unwrap(); self.list.IASetVertexBuffers(0,Some(&[D3D12_VERTEX_BUFFER_VIEW { BufferLocation:buffer.resource.GetGPUVirtualAddress(),SizeInBytes:buffer.size as u32,StrideInBytes:36 }])); self.list.DrawInstanced(6,range.end-range.start,0,range.start);
            }
        };
        draw_layer(&commands[0]); if !rects.is_empty() { let buffer=slot.rects.as_ref().unwrap(); self.list.SetPipelineState(&self.rects); self.list.IASetVertexBuffers(0,Some(&[D3D12_VERTEX_BUFFER_VIEW { BufferLocation:buffer.resource.GetGPUVirtualAddress(),SizeInBytes:buffer.size as u32,StrideInBytes:36 }])); self.list.DrawInstanced(6,rects.len() as u32,0,0); } draw_layer(&commands[1]);
        barrier(&self.list,&self.targets[target],D3D12_RESOURCE_STATE_RENDER_TARGET,D3D12_RESOURCE_STATE_PRESENT); self.list.Close().map_err(err)?; self.queue.ExecuteCommandLists(&[Some(self.list.cast().map_err(err)?)]);
        self.swapchain.Present(1,DXGI_PRESENT(0)).ok().map_err(err)?; self.queue.Signal(&self.fence,self.next_fence).map_err(err)?; self.slots[index].fence=self.next_fence; self.next_fence+=1; self.slot=(self.slot+1)%3; Ok(())
    } }
}
impl Drop for Gpu { fn drop(&mut self) { unsafe { for slot in &self.slots { let _=self.wait_fence(slot.fence); } let _=CloseHandle(self.event); } } }
