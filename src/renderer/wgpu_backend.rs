use std::ops::Range;
use std::sync::{Arc, LazyLock};

use crossfont::Metrics;
use log::{debug, info, warn};
use pollster::block_on;
use winit::window::Window as WinitWindow;

use crate::terminal::index::Point;
use crate::terminal::term::cell::Flags;
use unicode_width::UnicodeWidthChar;

use crate::display::SizeInfo;
use crate::display::color::{Rgb, srgb_byte_to_linear};
use crate::display::content::RenderableCell;
use crate::renderer::Error;
use crate::renderer::rects::{RectKind, RenderRect};

mod atlas;
mod builtin_font;
mod glyph_cache;

pub(crate) use glyph_cache::GlyphCache;

use atlas::{Glyph, GlyphAtlas};

// 着色器源码

const TEXT_SHADER: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/wgsl/text.wgsl"));
const RECT_SHADER: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/wgsl/rect.wgsl"));

/// 文本实例数据, 与 WGSL 中的 VertexInput 对应.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct TextInstanceData {
    // grid_coords: col, row
    grid: [u16; 2],
    // glyph: left, top, width, height
    glyph: [i16; 4],
    // uv: uv_left, uv_bot, uv_width, uv_height
    uv: [f32; 4],
    // text_color: r, g, b, cell_flags
    text_color: [u8; 4],
    // bg_color: r, g, b, a
    bg_color: [u8; 4],
}

const _: () = assert!(std::mem::size_of::<TextInstanceData>() == 36);

/// 文本相对于矩形层的渲染位置.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TextLayer {
    BeforeRects,
    AfterRects,
}

impl TextLayer {
    fn index(self) -> usize {
        match self {
            Self::BeforeRects => 0,
            Self::AfterRects => 1,
        }
    }
}

/// Reason a surface frame is temporarily unavailable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FrameUnavailable {
    Retry,
    Suspended,
}

/// Surface resources which live for one rendered frame.
pub(crate) struct WgpuFrame {
    output: wgpu::SurfaceTexture,
    view: wgpu::TextureView,
    encoder: wgpu::CommandEncoder,
}

#[derive(Default)]
struct TextBatch {
    instances_by_atlas: Vec<Vec<TextInstanceData>>,
}

impl TextBatch {
    fn clear(&mut self) {
        for instances in &mut self.instances_by_atlas {
            instances.clear();
        }
    }
}

enum TextDrawCommand {
    Background(Range<u32>),
    Glyph {
        atlas_index: usize,
        instance_range: Range<u32>,
    },
}

/// Per-frame uniforms shared by text and rectangle pipelines.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct FrameUniforms {
    // projection: offset_x, offset_y, scale_x, scale_y
    projection: [f32; 4],
    // cell_dim: cell_width, cell_height
    cell_dim: [f32; 2],
    // Content viewport origin in framebuffer pixels.
    padding: [f32; 2],
    underline_position: f32,
    underline_thickness: f32,
    undercurl_position: f32,
    _pad: f32,
}

/// Per-rectangle instance data consumed by the rectangle vertex shader.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct RectInstanceData {
    // x, y, width, height in content-viewport pixels.
    position_size: [f32; 4],
    // Linear RGBA.
    color: [f32; 4],
    kind: u32,
}

const _: () = assert!(std::mem::size_of::<FrameUniforms>() == 48);
const _: () = assert!(std::mem::size_of::<RectInstanceData>() == 36);

/// 初始文本实例 buffer 容量.
///
/// 超出时会按需扩容, 避免启动时为极端场景固定分配 0x1_0000 个实例的 GPU buffer.
const INITIAL_INSTANCE_CAPACITY: usize = 8192;
const INITIAL_RECT_INSTANCE_CAPACITY: usize = 1024;

/// Rendering glyph flags - 与着色器保持同步
const COLORED_FLAG: u32 = 1;
const WIDE_CHAR_FLAG: u32 = 2;

/// 将 sRGB 值转换为线性空间.
/// sRGB 颜色在传递给 GPU 前需要进行此转换, 否则颜色会偏浅.
/// 使用标准 sRGB 分段公式, 替代近似 powf(2.2).
static SRGB_TO_LINEAR_U8: LazyLock<[u8; 256]> = LazyLock::new(|| {
    std::array::from_fn(|value| {
        (srgb_byte_to_linear(value as u8) * 255.0)
            .round()
            .clamp(0.0, 255.0) as u8
    })
});

#[inline]
fn srgb_to_linear(srgb: u8) -> u8 {
    SRGB_TO_LINEAR_U8[srgb as usize]
}

/// f32 版本的 sRGB 到线性空间转换
#[inline]
fn srgb_to_linear_f32(srgb: u8) -> f32 {
    srgb_byte_to_linear(srgb)
}

pub(crate) struct WgpuRenderer {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    surface: wgpu::Surface<'static>,
    surface_target: Arc<WinitWindow>,
    surface_config: wgpu::SurfaceConfiguration,
    surface_config_dirty: bool,
    surface_opacity: f32,

    device: wgpu::Device,
    queue: wgpu::Queue,

    // -- 文本渲染管线 --
    text_bg_pipeline: wgpu::RenderPipeline,
    text_fg_pipeline: wgpu::RenderPipeline,
    frame_uniform_buffer: wgpu::Buffer,
    frame_uniform_bind_group: wgpu::BindGroup,
    text_instance_buffer: wgpu::Buffer,
    text_instance_buffer_capacity: usize,
    text_batches: [Vec<TextBatch>; 2],
    text_batch_counts: [usize; 2],
    text_instances: Vec<TextInstanceData>,
    text_draw_commands: [Vec<TextDrawCommand>; 2],

    // -- 矩形渲染管线 --
    rect_pipeline: wgpu::RenderPipeline,
    rect_instance_buffer: wgpu::Buffer,
    rect_instances: Vec<RectInstanceData>,

    // -- Atlas / 字形管理 --
    glyph_atlas: GlyphAtlas,
}

impl std::fmt::Debug for WgpuRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WgpuRenderer").finish_non_exhaustive()
    }
}

impl WgpuRenderer {
    pub(crate) fn new(surface_target: Arc<WinitWindow>, opacity: f32) -> Result<Self, Error> {
        let mut instance_descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        #[cfg(windows)]
        {
            instance_descriptor.backends = wgpu::Backends::DX12;
            instance_descriptor.backend_options.dx12.shader_compiler =
                wgpu::Dx12Compiler::default_dynamic_dxc();
            if opacity < 1.0 {
                instance_descriptor.backend_options.dx12.presentation_system =
                    wgpu::Dx12SwapchainKind::DxgiFromVisual;
            }
        }
        #[cfg(target_os = "macos")]
        {
            instance_descriptor.backends = wgpu::Backends::METAL;
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            instance_descriptor.backends = wgpu::Backends::VULKAN;
        }

        let instance = wgpu::Instance::new(instance_descriptor);
        let surface = Self::create_surface(&instance, &surface_target)?;
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .map_err(Error::RequestAdapter)?;
        info!("wgpu adapter: {:?}", adapter.get_info());

        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("alacritty_device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            ..Default::default()
        }))
        .map_err(Error::RequestDevice)?;

        let surface_config =
            Self::surface_configuration(&surface, &adapter, surface_target.inner_size(), opacity)?;
        surface.configure(&device, &surface_config);
        let surface_format = surface_config.format;

        info!("正在初始化 wgpu 渲染器");

        let text_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("text_shader"),
            source: wgpu::ShaderSource::Wgsl(TEXT_SHADER.into()),
        });

        // 文本 uniform 共享一个 buffer (bg 和 text 内容相同)
        let frame_uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("frame_uniform_buffer"),
            size: std::mem::size_of::<FrameUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let frame_uniform_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("frame_uniform_bind_group_layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        let frame_uniform_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("frame_uniform_bind_group"),
            layout: &frame_uniform_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: frame_uniform_buffer.as_entire_binding(),
            }],
        });

        // =============================
        // 纹理 bind group layout (用于 atlas)
        // =============================
        let text_texture_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("text_texture_bind_group_layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });

        let text_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("glyph_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        // =============================
        // 文本管线 layout
        // =============================
        let text_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("text_pipeline_layout"),
            bind_group_layouts: &[
                Some(&frame_uniform_bind_group_layout),
                Some(&text_texture_bind_group_layout),
            ],
            immediate_size: 0,
        });

        // 实例 buffer 的顶点布局
        let text_instance_layout = Some(wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<TextInstanceData>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &[
                // grid_coords: col, row
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Uint16x2,
                    offset: 0,
                    shader_location: 0,
                },
                // glyph: left, top, width, height
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Sint16x4,
                    offset: 4,
                    shader_location: 1,
                },
                // uv: uv_left, uv_bot, uv_width, uv_height
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x4,
                    offset: 12,
                    shader_location: 2,
                },
                // text_color: r, g, b, cell_flags
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Uint8x4,
                    offset: 28,
                    shader_location: 3,
                },
                // bg_color: r, g, b, a
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Unorm8x4,
                    offset: 32,
                    shader_location: 4,
                },
            ],
        });

        let create_text_pipeline = |label: &'static str,
                                    vertex_entry: &'static str,
                                    fragment_entry: &'static str,
                                    blend: wgpu::BlendState| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&text_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &text_shader,
                    entry_point: Some(vertex_entry),
                    buffers: std::slice::from_ref(&text_instance_layout),
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &text_shader,
                    entry_point: Some(fragment_entry),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: surface_format,
                        blend: Some(blend),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    cull_mode: None,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };

        let premultiplied_blend = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
        };
        let text_bg_pipeline =
            create_text_pipeline("text_bg_pipeline", "vs_bg", "fs_bg", premultiplied_blend);

        let text_fg_pipeline = create_text_pipeline(
            "text_fg_pipeline",
            "vs_text",
            "fs_text",
            wgpu::BlendState::ALPHA_BLENDING,
        );

        // =============================
        // 文本 instance buffer
        // =============================
        let text_instance_buffer =
            Self::create_text_instance_buffer(&device, INITIAL_INSTANCE_CAPACITY);

        // =============================
        // 矩形渲染管线
        // =============================
        let rect_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rect_shader"),
            source: wgpu::ShaderSource::Wgsl(RECT_SHADER.into()),
        });

        let rect_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rect_pipeline_layout"),
            bind_group_layouts: &[Some(&frame_uniform_bind_group_layout)],
            immediate_size: 0,
        });

        let rect_instance_layout = Some(wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<RectInstanceData>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &[
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x4,
                    offset: 0,
                    shader_location: 0,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x4,
                    offset: 16,
                    shader_location: 1,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Uint32,
                    offset: 32,
                    shader_location: 2,
                },
            ],
        });

        let rect_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rect_pipeline"),
            layout: Some(&rect_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &rect_shader,
                entry_point: Some("vs_main"),
                buffers: std::slice::from_ref(&rect_instance_layout),
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &rect_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let rect_instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rect_instance_buffer"),
            size: (INITIAL_RECT_INSTANCE_CAPACITY * std::mem::size_of::<RectInstanceData>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let glyph_atlas = GlyphAtlas::new(
            device.clone(),
            queue.clone(),
            text_texture_bind_group_layout,
            text_sampler,
        );

        info!("wgpu 渲染器初始化完成");

        Ok(Self {
            instance,
            adapter,
            surface,
            surface_target,
            surface_config,
            surface_config_dirty: false,
            surface_opacity: opacity,
            device,
            queue,

            text_bg_pipeline,
            text_fg_pipeline,
            frame_uniform_buffer,
            frame_uniform_bind_group,
            text_instance_buffer,
            text_instance_buffer_capacity: INITIAL_INSTANCE_CAPACITY,
            text_batches: Default::default(),
            text_batch_counts: [0; 2],
            text_instances: Vec::new(),
            text_draw_commands: Default::default(),

            rect_pipeline,
            rect_instance_buffer,
            rect_instances: Vec::new(),

            glyph_atlas,
        })
    }

    fn create_surface(
        instance: &wgpu::Instance,
        surface_target: &Arc<WinitWindow>,
    ) -> Result<wgpu::Surface<'static>, Error> {
        instance
            .create_surface(Arc::clone(surface_target))
            .map_err(Error::CreateSurface)
    }

    fn surface_configuration(
        surface: &wgpu::Surface<'_>,
        adapter: &wgpu::Adapter,
        size: winit::dpi::PhysicalSize<u32>,
        opacity: f32,
    ) -> Result<wgpu::SurfaceConfiguration, Error> {
        let caps = surface.get_capabilities(adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .or_else(|| caps.formats.first().copied())
            .ok_or(Error::MissingSurfaceCapability("texture formats"))?;
        let alpha_mode = Self::select_alpha_mode(&caps.alpha_modes, opacity)
            .ok_or(Error::MissingSurfaceCapability("alpha modes"))?;
        let present_mode = caps
            .present_modes
            .iter()
            .copied()
            .find(|mode| *mode == wgpu::PresentMode::AutoVsync)
            .or_else(|| caps.present_modes.first().copied())
            .ok_or(Error::MissingSurfaceCapability("present modes"))?;

        info!("wgpu surface format: {format:?}");
        info!("wgpu alpha mode: {alpha_mode:?}");
        info!("wgpu present mode: {present_mode:?}");

        Ok(wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            view_formats: vec![],
            alpha_mode,
            width: size.width.max(1),
            height: size.height.max(1),
            desired_maximum_frame_latency: 1,
            present_mode,
        })
    }

    fn select_alpha_mode(
        alpha_modes: &[wgpu::CompositeAlphaMode],
        opacity: f32,
    ) -> Option<wgpu::CompositeAlphaMode> {
        if opacity < 1.0 {
            alpha_modes
                .iter()
                .copied()
                .find(|mode| *mode == wgpu::CompositeAlphaMode::PreMultiplied)
                .or_else(|| {
                    alpha_modes
                        .iter()
                        .copied()
                        .find(|mode| *mode == wgpu::CompositeAlphaMode::PostMultiplied)
                })
                .or_else(|| alpha_modes.first().copied())
        } else {
            alpha_modes
                .iter()
                .copied()
                .find(|mode| *mode == wgpu::CompositeAlphaMode::Opaque)
                .or_else(|| alpha_modes.first().copied())
        }
    }

    fn configure_surface(&mut self, force: bool) -> bool {
        let size = self.surface_target.inner_size();
        if size.width == 0 || size.height == 0 {
            return false;
        }

        let dimensions_changed =
            self.surface_config.width != size.width || self.surface_config.height != size.height;
        if !force && !dimensions_changed && !self.surface_config_dirty {
            return true;
        }

        self.surface_config.width = size.width;
        self.surface_config.height = size.height;
        self.surface.configure(&self.device, &self.surface_config);
        self.surface_config_dirty = false;
        true
    }

    fn recreate_surface(&mut self) -> Result<(), Error> {
        let surface = Self::create_surface(&self.instance, &self.surface_target)?;
        let caps = surface.get_capabilities(&self.adapter);
        if !caps.formats.contains(&self.surface_config.format) {
            return Err(Error::MissingSurfaceCapability(
                "previously selected texture format",
            ));
        }
        self.surface_config.alpha_mode =
            Self::select_alpha_mode(&caps.alpha_modes, self.surface_opacity)
                .ok_or(Error::MissingSurfaceCapability("alpha modes"))?;
        self.surface = surface;
        self.surface_config_dirty = true;
        Ok(())
    }

    fn frame_from_output(&self, output: wgpu::SurfaceTexture) -> WgpuFrame {
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let encoder = self.device.create_command_encoder(&Default::default());
        WgpuFrame {
            output,
            view,
            encoder,
        }
    }

    fn acquire_after_recovery(&mut self) -> Result<WgpuFrame, FrameUnavailable> {
        match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(output) => Ok(self.frame_from_output(output)),
            wgpu::CurrentSurfaceTexture::Suboptimal(output) => {
                self.surface_config_dirty = true;
                Ok(self.frame_from_output(output))
            }
            wgpu::CurrentSurfaceTexture::Occluded => Err(FrameUnavailable::Suspended),
            status => {
                debug!("wgpu frame acquisition failed after recovery: {status:?}");
                Err(FrameUnavailable::Retry)
            }
        }
    }

    /// Acquire a frame and recover outdated or lost surfaces once.
    pub(crate) fn begin_frame(&mut self) -> Result<WgpuFrame, FrameUnavailable> {
        self.text_batch_counts = [0; 2];

        if !self.configure_surface(false) {
            return Err(FrameUnavailable::Suspended);
        }

        match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(output) => Ok(self.frame_from_output(output)),
            wgpu::CurrentSurfaceTexture::Suboptimal(output) => {
                self.surface_config_dirty = true;
                Ok(self.frame_from_output(output))
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                if self.configure_surface(true) {
                    self.acquire_after_recovery()
                } else {
                    Err(FrameUnavailable::Suspended)
                }
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                if let Err(err) = self.recreate_surface() {
                    warn!("Failed to recreate lost wgpu surface: {err}");
                    return Err(FrameUnavailable::Retry);
                }
                if self.configure_surface(true) {
                    self.acquire_after_recovery()
                } else {
                    Err(FrameUnavailable::Suspended)
                }
            }
            wgpu::CurrentSurfaceTexture::Timeout => {
                debug!("wgpu frame acquisition timed out");
                Err(FrameUnavailable::Retry)
            }
            wgpu::CurrentSurfaceTexture::Occluded => Err(FrameUnavailable::Suspended),
            wgpu::CurrentSurfaceTexture::Validation => {
                warn!("wgpu validation error while acquiring surface texture");
                Err(FrameUnavailable::Retry)
            }
        }
    }

    pub(crate) fn render_frame(
        &mut self,
        frame: &mut WgpuFrame,
        size_info: &SizeInfo,
        metrics: &Metrics,
        rects: &[RenderRect],
        clear_color: Rgb,
        clear_alpha: f32,
    ) {
        self.update_frame_uniforms(size_info, metrics);
        self.prepare_text();
        self.prepare_rects(rects);
        let clear_color = Self::clear_color(clear_color, clear_alpha);

        let mut rpass = frame
            .encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("frame_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &frame.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_color),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        let (viewport_x, viewport_y, viewport_width, viewport_height) =
            Self::content_viewport(size_info);
        rpass.set_viewport(
            viewport_x,
            viewport_y,
            viewport_width,
            viewport_height,
            0.0,
            1.0,
        );

        self.render_text_layer(
            &self.text_draw_commands[TextLayer::BeforeRects.index()],
            &mut rpass,
        );
        if !self.rect_instances.is_empty() {
            self.render_rects(&mut rpass);
        }
        self.render_text_layer(
            &self.text_draw_commands[TextLayer::AfterRects.index()],
            &mut rpass,
        );
    }

    pub(crate) fn submit_frame(&self, frame: WgpuFrame) {
        self.queue.submit(std::iter::once(frame.encoder.finish()));
        self.queue.present(frame.output);
    }

    pub(crate) fn present_clear(&mut self, color: Rgb, alpha: f32) {
        let Ok(mut frame) = self.begin_frame() else {
            return;
        };
        self.clear_frame(&mut frame, color, alpha);
        self.submit_frame(frame);
    }

    fn clear_frame(&self, frame: &mut WgpuFrame, color: Rgb, alpha: f32) {
        let _rpass = frame
            .encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("clear_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &frame.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(Self::clear_color(color, alpha)),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
    }

    fn clear_color(color: Rgb, alpha: f32) -> wgpu::Color {
        wgpu::Color {
            r: (srgb_to_linear_f32(color.r) * alpha) as f64,
            g: (srgb_to_linear_f32(color.g) * alpha) as f64,
            b: (srgb_to_linear_f32(color.b) * alpha) as f64,
            a: alpha as f64,
        }
    }

    fn content_viewport(size_info: &SizeInfo) -> (f32, f32, f32, f32) {
        let x = size_info.padding_x();
        let y = size_info.padding_y();
        let width = (size_info.width() - 2.0 * x).max(1.0);
        let height = (size_info.height() - 2.0 * y).max(1.0);
        (x, y, width, height)
    }

    fn create_text_instance_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("text_instance_buffer"),
            size: (capacity * std::mem::size_of::<TextInstanceData>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    fn ensure_text_instance_buffer_capacity(&mut self, required: usize) {
        if required <= self.text_instance_buffer_capacity {
            return;
        }

        let capacity = required.next_power_of_two();
        self.text_instance_buffer = Self::create_text_instance_buffer(&self.device, capacity);
        self.text_instance_buffer_capacity = capacity;
    }

    /// 将一批单元格加入指定文本层, 不立即录制绘制命令.
    pub(crate) fn queue_cells<I>(
        &mut self,
        layer: TextLayer,
        glyph_cache: &mut GlyphCache,
        cells: I,
    ) where
        I: IntoIterator<Item = RenderableCell>,
    {
        let cells = cells.into_iter();
        let layer_index = layer.index();
        let batch_index = self.text_batch_counts[layer_index];
        if batch_index == self.text_batches[layer_index].len() {
            self.text_batches[layer_index].push(TextBatch::default());
        }

        let batch = &mut self.text_batches[layer_index][batch_index];
        batch.clear();
        let minimum_instances = cells.size_hint().0;
        Self::ensure_instance_group(&mut batch.instances_by_atlas, 0).reserve(minimum_instances);

        for cell in cells {
            Self::process_cell(
                &mut self.glyph_atlas,
                cell,
                glyph_cache,
                &mut batch.instances_by_atlas,
            );
        }

        if batch
            .instances_by_atlas
            .iter()
            .any(|instances| !instances.is_empty())
        {
            self.text_batch_counts[layer_index] += 1;
        }
    }

    /// Flatten and upload all text queued for the current frame.
    fn prepare_text(&mut self) {
        self.flatten_text_batches();
        let total_instances = self.text_instances.len();
        if total_instances == 0 {
            return;
        }

        self.ensure_text_instance_buffer_capacity(total_instances);
        self.queue.write_buffer(
            &self.text_instance_buffer,
            0,
            bytemuck::cast_slice(&self.text_instances),
        );
    }

    fn flatten_text_batches(&mut self) {
        self.text_instances.clear();
        for commands in &mut self.text_draw_commands {
            commands.clear();
        }

        for (layer_index, batches) in self.text_batches.iter().enumerate() {
            let commands = &mut self.text_draw_commands[layer_index];
            for batch in batches.iter().take(self.text_batch_counts[layer_index]) {
                let batch_start = self.text_instances.len() as u32;
                let command_start = commands.len();
                commands.push(TextDrawCommand::Background(batch_start..batch_start));

                for (atlas_index, instances) in batch.instances_by_atlas.iter().enumerate() {
                    if instances.is_empty() {
                        continue;
                    }

                    let start = self.text_instances.len() as u32;
                    self.text_instances.extend_from_slice(instances);
                    let end = self.text_instances.len() as u32;
                    commands.push(TextDrawCommand::Glyph {
                        atlas_index,
                        instance_range: start..end,
                    });
                }

                let batch_end = self.text_instances.len() as u32;
                if batch_start == batch_end {
                    commands.truncate(command_start);
                } else if let TextDrawCommand::Background(instance_range) =
                    &mut commands[command_start]
                {
                    *instance_range = batch_start..batch_end;
                }
            }
        }
    }

    fn render_text_layer<'pass>(
        &'pass self,
        commands: &'pass [TextDrawCommand],
        rpass: &mut wgpu::RenderPass<'pass>,
    ) {
        if commands.is_empty() {
            return;
        }

        rpass.set_bind_group(0, &self.frame_uniform_bind_group, &[]);
        rpass.set_vertex_buffer(0, self.text_instance_buffer.slice(..));

        let mut foreground_pipeline_active = false;
        for command in commands {
            match command {
                TextDrawCommand::Background(instance_range) => {
                    rpass.set_pipeline(&self.text_bg_pipeline);
                    rpass.set_bind_group(1, self.glyph_atlas.bind_group(0), &[]);
                    rpass.draw(0..6, instance_range.clone());
                    foreground_pipeline_active = false;
                }
                TextDrawCommand::Glyph {
                    atlas_index,
                    instance_range,
                } => {
                    if !foreground_pipeline_active {
                        rpass.set_pipeline(&self.text_fg_pipeline);
                        foreground_pipeline_active = true;
                    }
                    rpass.set_bind_group(1, self.glyph_atlas.bind_group(*atlas_index), &[]);
                    rpass.draw(0..6, instance_range.clone());
                }
            }
        }
    }

    fn process_cell(
        glyph_atlas: &mut GlyphAtlas,
        mut cell: RenderableCell,
        glyph_cache: &mut GlyphCache,
        instances_by_atlas: &mut Vec<Vec<TextInstanceData>>,
    ) {
        // 隐藏的单元格和 tab 渲染为空格.
        let hidden = cell.flags.contains(Flags::HIDDEN);
        if cell.character == '\t' || hidden {
            cell.character = ' ';
        }

        let glyph = glyph_cache.get(cell.character, cell.flags, glyph_atlas, true);
        let instance = Self::create_instance(&cell, &glyph);
        Self::ensure_instance_group(instances_by_atlas, glyph.atlas_index).push(instance);

        // 渲染可见的零宽字符.
        if let Some(zerowidth) = cell
            .extra
            .as_mut()
            .and_then(|extra| extra.zerowidth.take().filter(|_| !hidden))
        {
            for character in zerowidth {
                let glyph = glyph_cache.get(character, cell.flags, glyph_atlas, false);
                let mut zerowidth_cell = cell.clone();
                zerowidth_cell.bg_alpha = 0.0;
                let instance = Self::create_instance(&zerowidth_cell, &glyph);
                Self::ensure_instance_group(instances_by_atlas, glyph.atlas_index).push(instance);
            }
        }
    }

    fn ensure_instance_group(
        instances_by_atlas: &mut Vec<Vec<TextInstanceData>>,
        atlas_index: usize,
    ) -> &mut Vec<TextInstanceData> {
        if instances_by_atlas.len() <= atlas_index {
            instances_by_atlas.resize_with(atlas_index + 1, Vec::new);
        }

        &mut instances_by_atlas[atlas_index]
    }

    fn create_instance(cell: &RenderableCell, glyph: &Glyph) -> TextInstanceData {
        let mut cell_flags: u8 = 0;
        if glyph.multicolor {
            cell_flags |= COLORED_FLAG as u8;
        }
        if cell.flags.contains(Flags::WIDE_CHAR) {
            cell_flags |= WIDE_CHAR_FLAG as u8;
        }

        let bg_alpha = (cell.bg_alpha.clamp(0.0, 1.0) * 255.0).round() as u8;

        TextInstanceData {
            grid: [cell.point.column.0 as u16, cell.point.line as u16],
            glyph: [glyph.left, glyph.top, glyph.width, glyph.height],
            uv: [glyph.uv_left, glyph.uv_bot, glyph.uv_width, glyph.uv_height],
            // 将 sRGB 颜色转换为线性空间
            text_color: [
                srgb_to_linear(cell.fg.r),
                srgb_to_linear(cell.fg.g),
                srgb_to_linear(cell.fg.b),
                cell_flags,
            ],
            // 将 sRGB 颜色转换为线性空间
            bg_color: [
                srgb_to_linear(cell.bg.r),
                srgb_to_linear(cell.bg.g),
                srgb_to_linear(cell.bg.b),
                bg_alpha,
            ],
        }
    }

    fn update_frame_uniforms(&self, size: &SizeInfo, metrics: &Metrics) {
        let (_, _, drawable_width, drawable_height) = Self::content_viewport(size);
        let uniforms = FrameUniforms {
            projection: [-1.0, 1.0, 2.0 / drawable_width, -2.0 / drawable_height],
            cell_dim: [size.cell_width(), size.cell_height()],
            padding: [size.padding_x(), size.padding_y()],
            underline_position: metrics.descent.abs() - metrics.underline_position.abs(),
            underline_thickness: metrics.underline_thickness,
            undercurl_position: (0.5 * metrics.descent).abs(),
            _pad: 0.0,
        };
        self.queue
            .write_buffer(&self.frame_uniform_buffer, 0, bytemuck::bytes_of(&uniforms));
    }

    /// 将字符串加入指定文本层, 不立即录制绘制命令.
    pub(crate) fn queue_string(
        &mut self,
        layer: TextLayer,
        point: Point<usize>,
        fg: Rgb,
        bg: Rgb,
        string_chars: impl Iterator<Item = char>,
        glyph_cache: &mut GlyphCache,
    ) {
        let mut column = point.column;
        let cells = string_chars.map(|character| {
            let width = character.width().unwrap_or(1).max(1);
            let flags = if width > 1 {
                Flags::WIDE_CHAR
            } else {
                Flags::empty()
            };
            let cell = RenderableCell {
                point: Point::new(point.line, column),
                character,
                extra: None,
                flags,
                bg_alpha: 1.0,
                fg,
                bg,
                underline: fg,
            };
            column += width;
            cell
        });

        self.queue_cells(layer, glyph_cache, cells);
    }

    /// Build and upload rectangle instances for the current frame.
    fn prepare_rects(&mut self, rects: &[RenderRect]) {
        self.rect_instances.clear();
        self.rect_instances.reserve(rects.len());

        // Preserve the existing painter order, with normal rectangles on top.
        for kind in [
            RectKind::DashedUnderline,
            RectKind::DottedUnderline,
            RectKind::Undercurl,
            RectKind::Normal,
        ] {
            self.rect_instances.extend(
                rects
                    .iter()
                    .filter(|rect| rect.kind == kind)
                    .map(Self::create_rect_instance),
            );
        }

        if self.rect_instances.is_empty() {
            return;
        }

        let needed_size =
            (self.rect_instances.len() * std::mem::size_of::<RectInstanceData>()) as u64;
        if needed_size > self.rect_instance_buffer.size() {
            self.rect_instance_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("rect_instance_buffer_resized"),
                size: needed_size.next_power_of_two(),
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        self.queue.write_buffer(
            &self.rect_instance_buffer,
            0,
            bytemuck::cast_slice(&self.rect_instances),
        );
    }

    fn render_rects<'pass>(&'pass self, rpass: &mut wgpu::RenderPass<'pass>) {
        rpass.set_pipeline(&self.rect_pipeline);
        rpass.set_bind_group(0, &self.frame_uniform_bind_group, &[]);
        rpass.set_vertex_buffer(0, self.rect_instance_buffer.slice(..));
        rpass.draw(0..6, 0..self.rect_instances.len() as u32);
    }

    fn create_rect_instance(rect: &RenderRect) -> RectInstanceData {
        RectInstanceData {
            position_size: [rect.x, rect.y, rect.width, rect.height],
            color: [
                srgb_to_linear_f32(rect.color.r),
                srgb_to_linear_f32(rect.color.g),
                srgb_to_linear_f32(rect.color.b),
                rect.alpha,
            ],
            kind: rect.kind as u32,
        }
    }

    /// 清空 atlas 并使用当前字体配置重新预取常用字形.
    pub(crate) fn reset_glyph_cache(&mut self, glyph_cache: &mut GlyphCache) {
        glyph_cache.reset_glyph_cache(&mut self.glyph_atlas);
    }
}
