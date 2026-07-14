use std::ops::Range;
use std::sync::Arc;

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
use crate::renderer::rects::RenderRect;

mod atlas;
mod glyph_cache;

pub(crate) use crate::renderer::text::builtin_font;

pub use glyph_cache::GlyphCache;

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
pub enum TextLayer {
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
pub enum FrameUnavailable {
    Retry,
    Suspended,
}

/// Surface resources which live for one rendered frame.
pub struct WgpuFrame {
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

struct TextDrawBatch {
    instance_range: Range<u32>,
    atlas_ranges: Vec<(usize, Range<u32>)>,
}

/// 文本 uniform 数据
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct TextUniforms {
    // projection: offset_x, offset_y, scale_x, scale_y
    projection: [f32; 4],
    // cell_dim: cell_width, cell_height
    cell_dim: [f32; 2],
    _pad: [f32; 2],
}

/// 矩形顶点数据
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct RectVertex {
    // NDC 坐标
    x: f32,
    y: f32,
    // 颜色 (归一化)
    r: f32,
    g: f32,
    b: f32,
    a: f32,
}

/// 矩形 uniform 数据
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct RectUniforms {
    cell_width: f32,
    cell_height: f32,
    padding_x: f32,
    padding_y: f32,
    underline_position: f32,
    underline_thickness: f32,
    undercurl_position: f32,
    _pad: f32,
}

/// 初始文本实例 buffer 容量.
///
/// 超出时会按需扩容, 避免启动时为极端场景固定分配 0x1_0000 个实例的 GPU buffer.
const INITIAL_INSTANCE_CAPACITY: usize = 8192;

/// Rendering glyph flags - 与着色器保持同步
const COLORED_FLAG: u32 = 1;
const WIDE_CHAR_FLAG: u32 = 2;

/// 将 sRGB 值转换为线性空间.
/// sRGB 颜色在传递给 GPU 前需要进行此转换, 否则颜色会偏浅.
/// 使用标准 sRGB 分段公式, 替代近似 powf(2.2).
#[inline]
fn srgb_to_linear(srgb: u8) -> u8 {
    (srgb_byte_to_linear(srgb) * 255.0)
        .round()
        .clamp(0.0, 255.0) as u8
}

/// f32 版本的 sRGB 到线性空间转换
#[inline]
fn srgb_to_linear_f32(srgb: u8) -> f32 {
    srgb_byte_to_linear(srgb)
}

pub struct WgpuRenderer {
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
    text_uniform_buffer: wgpu::Buffer,
    text_uniform_bind_group: wgpu::BindGroup,
    text_instance_buffer: wgpu::Buffer,
    text_instance_buffer_capacity: usize,
    text_batches: [Vec<TextBatch>; 2],
    text_batch_counts: [usize; 2],
    text_instances: Vec<TextInstanceData>,

    // -- 矩形渲染管线 --
    rect_pipelines: [wgpu::RenderPipeline; 4], // normal, undercurl, dotted, dashed
    rect_uniform_buffer: wgpu::Buffer,
    rect_uniform_bind_group: wgpu::BindGroup,
    rect_vertex_buffer: wgpu::Buffer,

    // -- Atlas / 字形管理 --
    glyph_atlas: GlyphAtlas,
}

impl std::fmt::Debug for WgpuRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WgpuRenderer").finish_non_exhaustive()
    }
}

impl WgpuRenderer {
    pub fn new(surface_target: Arc<WinitWindow>, opacity: f32) -> Result<Self, Error> {
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
        let text_uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("text_uniform_buffer"),
            size: std::mem::size_of::<TextUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let text_uniform_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("text_uniform_bind_group_layout"),
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

        let text_uniform_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("text_uniform_bind_group"),
            layout: &text_uniform_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: text_uniform_buffer.as_entire_binding(),
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
                Some(&text_uniform_bind_group_layout),
                Some(&text_texture_bind_group_layout),
            ],
            immediate_size: 0,
        });

        // 实例 buffer 的顶点布局
        let text_instance_layout = wgpu::VertexBufferLayout {
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
        };

        // 背景 pass 管线 - 使用预乘 alpha 混合
        let text_bg_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("text_bg_pipeline"),
            layout: Some(&text_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &text_shader,
                entry_point: Some("vs_bg"),
                buffers: std::slice::from_ref(&text_instance_layout),
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &text_shader,
                entry_point: Some("fs_bg"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState {
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
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        // 文字 pass 管线 - 使用标准 alpha 混合
        let text_fg_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("text_fg_pipeline"),
            layout: Some(&text_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &text_shader,
                entry_point: Some("vs_text"),
                buffers: &[text_instance_layout],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &text_shader,
                entry_point: Some("fs_text"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::SrcAlpha,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

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

        let rect_uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rect_uniform_buffer"),
            size: std::mem::size_of::<RectUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let rect_uniform_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("rect_uniform_bind_group_layout"),
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

        let rect_uniform_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rect_uniform_bind_group"),
            layout: &rect_uniform_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: rect_uniform_buffer.as_entire_binding(),
            }],
        });

        let rect_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rect_pipeline_layout"),
            bind_group_layouts: &[Some(&rect_uniform_bind_group_layout)],
            immediate_size: 0,
        });

        let rect_vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<RectVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                // position
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x2,
                    offset: 0,
                    shader_location: 0,
                },
                // color
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x4,
                    offset: 8,
                    shader_location: 1,
                },
            ],
        };

        let fs_entries = ["fs_normal", "fs_undercurl", "fs_dotted", "fs_dashed"];
        let rect_pipelines = std::array::from_fn(|i| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(&format!("rect_pipeline_{}", fs_entries[i])),
                layout: Some(&rect_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &rect_shader,
                    entry_point: Some("vs_main"),
                    buffers: std::slice::from_ref(&rect_vertex_layout),
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &rect_shader,
                    entry_point: Some(fs_entries[i]),
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
            })
        });

        // 矩形顶点 buffer - 预分配较大空间
        let rect_vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rect_vertex_buffer"),
            size: (4096 * std::mem::size_of::<RectVertex>()) as u64,
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
            text_uniform_buffer,
            text_uniform_bind_group,
            text_instance_buffer,
            text_instance_buffer_capacity: INITIAL_INSTANCE_CAPACITY,
            text_batches: Default::default(),
            text_batch_counts: [0; 2],
            text_instances: Vec::new(),

            rect_pipelines,
            rect_uniform_buffer,
            rect_uniform_bind_group,
            rect_vertex_buffer,

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
    pub fn begin_frame(&mut self) -> Result<WgpuFrame, FrameUnavailable> {
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

    #[cfg(unix)]
    pub fn update_surface_opacity(&mut self, opacity: f32) {
        self.surface_opacity = opacity;
        let caps = self.surface.get_capabilities(&self.adapter);
        let Some(alpha_mode) = Self::select_alpha_mode(&caps.alpha_modes, opacity) else {
            warn!("wgpu surface exposes no alpha modes");
            return;
        };

        if self.surface_config.alpha_mode != alpha_mode {
            self.surface_config.alpha_mode = alpha_mode;
            self.surface_config_dirty = true;
        }
    }

    pub fn clear_frame(&self, frame: &mut WgpuFrame, color: Rgb, alpha: f32) {
        self.clear(&mut frame.encoder, &frame.view, color, alpha);
    }

    pub fn render_frame(
        &mut self,
        frame: &mut WgpuFrame,
        size_info: &SizeInfo,
        metrics: &Metrics,
        rects: Vec<RenderRect>,
    ) {
        self.render_queued(size_info, metrics, rects, &mut frame.encoder, &frame.view);
    }

    pub fn submit_frame(&self, frame: WgpuFrame) {
        self.queue.submit(std::iter::once(frame.encoder.finish()));
        frame.output.present();
    }

    pub fn present_clear(&mut self, color: Rgb, alpha: f32) -> bool {
        let Ok(mut frame) = self.begin_frame() else {
            return false;
        };
        self.clear_frame(&mut frame, color, alpha);
        self.submit_frame(frame);
        true
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
    pub fn queue_cells<I>(&mut self, layer: TextLayer, glyph_cache: &mut GlyphCache, cells: I)
    where
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

    /// 一次性上传本帧所有文本实例, 并按文本层、矩形层的固定顺序录制命令.
    pub fn render_queued(
        &mut self,
        size_info: &SizeInfo,
        metrics: &Metrics,
        rects: Vec<RenderRect>,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
    ) {
        let draw_batches = self.flatten_text_batches();
        let total_instances = self.text_instances.len();

        if total_instances > 0 {
            let uniforms = self.compute_text_uniforms(size_info);
            self.queue
                .write_buffer(&self.text_uniform_buffer, 0, bytemuck::bytes_of(&uniforms));

            self.ensure_text_instance_buffer_capacity(total_instances);
            self.queue.write_buffer(
                &self.text_instance_buffer,
                0,
                bytemuck::cast_slice(&self.text_instances),
            );
        }

        self.render_text_layer(
            "text_before_rects_pass",
            size_info,
            &draw_batches[TextLayer::BeforeRects.index()],
            encoder,
            view,
        );
        self.draw_rects(size_info, metrics, rects, encoder, view);
        self.render_text_layer(
            "text_after_rects_pass",
            size_info,
            &draw_batches[TextLayer::AfterRects.index()],
            encoder,
            view,
        );

        self.text_batch_counts = [0; 2];
    }

    fn flatten_text_batches(&mut self) -> [Vec<TextDrawBatch>; 2] {
        self.text_instances.clear();
        let mut draw_batches: [Vec<TextDrawBatch>; 2] = Default::default();

        for (layer_index, batches) in self.text_batches.iter().enumerate() {
            for batch in batches.iter().take(self.text_batch_counts[layer_index]) {
                let batch_start = self.text_instances.len() as u32;
                let mut atlas_ranges = Vec::new();

                for (atlas_index, instances) in batch.instances_by_atlas.iter().enumerate() {
                    if instances.is_empty() {
                        continue;
                    }

                    let start = self.text_instances.len() as u32;
                    self.text_instances.extend_from_slice(instances);
                    let end = self.text_instances.len() as u32;
                    atlas_ranges.push((atlas_index, start..end));
                }

                let batch_end = self.text_instances.len() as u32;
                if batch_start != batch_end {
                    draw_batches[layer_index].push(TextDrawBatch {
                        instance_range: batch_start..batch_end,
                        atlas_ranges,
                    });
                }
            }
        }

        draw_batches
    }

    fn render_text_layer(
        &self,
        label: &'static str,
        size_info: &SizeInfo,
        draw_batches: &[TextDrawBatch],
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
    ) {
        if draw_batches.is_empty() {
            return;
        }

        let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some(label),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
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
        rpass.set_bind_group(0, &self.text_uniform_bind_group, &[]);
        rpass.set_vertex_buffer(0, self.text_instance_buffer.slice(..));

        for batch in draw_batches {
            rpass.set_pipeline(&self.text_bg_pipeline);
            rpass.set_bind_group(1, self.glyph_atlas.bind_group(0), &[]);
            rpass.draw(0..6, batch.instance_range.clone());

            rpass.set_pipeline(&self.text_fg_pipeline);
            for (atlas_index, instance_range) in &batch.atlas_ranges {
                rpass.set_bind_group(1, self.glyph_atlas.bind_group(*atlas_index), &[]);
                rpass.draw(0..6, instance_range.clone());
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

    fn compute_text_uniforms(&self, size: &SizeInfo) -> TextUniforms {
        let (_, _, drawable_width, drawable_height) = Self::content_viewport(size);

        let scale_x = 2. / drawable_width;
        let scale_y = -2. / drawable_height;
        let offset_x = -1.;
        let offset_y = 1.;

        TextUniforms {
            projection: [offset_x, offset_y, scale_x, scale_y],
            cell_dim: [size.cell_width(), size.cell_height()],
            _pad: [0.0; 2],
        }
    }

    /// 将字符串加入指定文本层, 不立即录制绘制命令.
    pub fn queue_string(
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

    /// 清屏
    pub fn clear(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        color: Rgb,
        alpha: f32,
    ) {
        // 将 sRGB 颜色转换为线性空间
        let r = srgb_to_linear_f32(color.r) * alpha;
        let g = srgb_to_linear_f32(color.g) * alpha;
        let b = srgb_to_linear_f32(color.b) * alpha;

        let _rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("clear_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: r as f64,
                        g: g as f64,
                        b: b as f64,
                        a: alpha as f64,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        // render pass 在 drop 时自动结束
    }

    /// 绘制矩形
    fn draw_rects(
        &mut self,
        size_info: &SizeInfo,
        metrics: &Metrics,
        rects: Vec<RenderRect>,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
    ) {
        if rects.is_empty() {
            return;
        }

        // 矩形和文本必须使用同一套内容区域投影, 否则 hollow cursor 等矩形会随行号
        // 逐渐偏离文本位置.
        let (_, _, drawable_width, drawable_height) = Self::content_viewport(size_info);
        let half_width = drawable_width / 2.;
        let half_height = drawable_height / 2.;

        // 按矩形类型分类顶点
        let mut vertices_by_kind: [Vec<RectVertex>; 4] = Default::default();
        let estimated_vertices_per_kind = rects.len().saturating_mul(6) / vertices_by_kind.len();
        for vertices in &mut vertices_by_kind {
            vertices.reserve(estimated_vertices_per_kind);
        }
        for rect in &rects {
            let kind_idx = rect.kind as usize;
            if kind_idx < 4 {
                Self::add_rect_vertices(
                    &mut vertices_by_kind[kind_idx],
                    half_width,
                    half_height,
                    rect,
                );
            }
        }

        // 计算 uniform 数据
        let position = (0.5 * metrics.descent).abs();
        let underline_position = metrics.descent.abs() - metrics.underline_position.abs();
        let padding_y = size_info.padding_y();

        let rect_uniforms = RectUniforms {
            cell_width: size_info.cell_width(),
            cell_height: size_info.cell_height(),
            padding_x: size_info.padding_x(),
            padding_y,
            underline_position,
            underline_thickness: metrics.underline_thickness,
            undercurl_position: position,
            _pad: 0.0,
        };
        self.queue.write_buffer(
            &self.rect_uniform_buffer,
            0,
            bytemuck::bytes_of(&rect_uniforms),
        );

        let mut all_vertices = Vec::with_capacity(rects.len().saturating_mul(6));
        let mut ranges = Vec::new();
        // 逆序绘制, 普通矩形在最上面.
        for kind_idx in (0..4).rev() {
            let vertices = &vertices_by_kind[kind_idx];
            if vertices.is_empty() {
                continue;
            }

            let start = all_vertices.len() as u32;
            all_vertices.extend_from_slice(vertices);
            let end = all_vertices.len() as u32;
            ranges.push((kind_idx, start, end));
        }

        if ranges.is_empty() {
            return;
        }

        let vertex_data = bytemuck::cast_slice(&all_vertices);
        let needed_size = vertex_data.len() as u64;
        if needed_size > self.rect_vertex_buffer.size() {
            let new_size = needed_size.next_power_of_two();
            self.rect_vertex_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("rect_vertex_buffer_resized"),
                size: new_size,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        self.queue
            .write_buffer(&self.rect_vertex_buffer, 0, vertex_data);

        let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("rect_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
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
        rpass.set_bind_group(0, &self.rect_uniform_bind_group, &[]);
        rpass.set_vertex_buffer(0, self.rect_vertex_buffer.slice(..));
        for (kind_idx, start, end) in ranges {
            rpass.set_pipeline(&self.rect_pipelines[kind_idx]);
            rpass.draw(start..end, 0..1);
        }
    }

    fn add_rect_vertices(
        vertices: &mut Vec<RectVertex>,
        half_width: f32,
        half_height: f32,
        rect: &RenderRect,
    ) {
        // NDC 范围从 -1 到 +1, Y 轴向上.
        let x = rect.x / half_width - 1.0;
        let y = -rect.y / half_height + 1.0;
        let width = rect.width / half_width;
        let height = rect.height / half_height;
        let (r, g, b) = rect.color.as_tuple();
        let a = rect.alpha;
        // 将 sRGB 颜色转换为线性空间
        let r = srgb_to_linear_f32(r);
        let g = srgb_to_linear_f32(g);
        let b = srgb_to_linear_f32(b);

        // 两个三角形构成一个四边形
        let quad = [
            RectVertex { x, y, r, g, b, a },
            RectVertex {
                x,
                y: y - height,
                r,
                g,
                b,
                a,
            },
            RectVertex {
                x: x + width,
                y,
                r,
                g,
                b,
                a,
            },
            RectVertex {
                x: x + width,
                y: y - height,
                r,
                g,
                b,
                a,
            },
        ];

        vertices.push(quad[0]);
        vertices.push(quad[1]);
        vertices.push(quad[2]);
        vertices.push(quad[2]);
        vertices.push(quad[3]);
        vertices.push(quad[1]);
    }

    /// 清空 atlas 并使用当前字体配置重新预取常用字形.
    pub fn reset_glyph_cache(&mut self, glyph_cache: &mut GlyphCache) {
        glyph_cache.reset_glyph_cache(&mut self.glyph_atlas);
    }
}
