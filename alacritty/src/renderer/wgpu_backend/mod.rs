// wgpu 渲染后端主模块
// 在 Windows 上替代 OpenGL 渲染, 使用 DX12 后端

use std::sync::Arc;

use crossfont::{Metrics, RasterizedGlyph};
use log::info;

use alacritty_terminal::index::Point;
use alacritty_terminal::term::cell::Flags;
use unicode_width::UnicodeWidthChar;

use crate::display::SizeInfo;
use crate::display::color::Rgb;
use crate::display::content::RenderableCell;
use crate::renderer::rects::RenderRect;

pub mod atlas;
pub mod glyph_cache;

// 重新导出 builtin_font, 使 glyph_cache 能引用
pub(crate) use crate::renderer::text::builtin_font;

pub use glyph_cache::{Glyph, GlyphCache, LoadGlyph};

use atlas::{ATLAS_SIZE, Atlas};

// 着色器源码
const TEXT_SHADER: &str = include_str!("../../../res/wgpu/text.wgsl");
const RECT_SHADER: &str = include_str!("../../../res/wgpu/rect.wgsl");

/// 文本实例数据, 与 WGSL 中的 VertexInput 对应.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct TextInstanceData {
    // grid_coords: col, row
    col: u32,
    row: u32,
    // glyph: left, top, width, height
    glyph_left: i32,
    glyph_top: i32,
    glyph_width: i32,
    glyph_height: i32,
    // uv: uv_left, uv_bot, uv_width, uv_height
    uv_left: f32,
    uv_bot: f32,
    uv_width: f32,
    uv_height: f32,
    // text_color: r, g, b, cell_flags
    text_r: u32,
    text_g: u32,
    text_b: u32,
    cell_flags: u32,
    // bg_color: r, g, b, a
    bg_r: u32,
    bg_g: u32,
    bg_b: u32,
    bg_a: u32,
}

/// 文本 uniform 数据
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct TextUniforms {
    // projection: offset_x, offset_y, scale_x, scale_y
    projection: [f32; 4],
    // cell_dim: cell_width, cell_height
    cell_dim: [f32; 2],
    // rendering_pass: 0 = background, 1 = text
    rendering_pass: i32,
    _pad: i32,
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
    rect_kind: i32,
}

/// 每个 atlas 对应的 bind group
struct AtlasBindGroup {
    bind_group: wgpu::BindGroup,
}

/// 最大批量实例数
const BATCH_MAX: usize = 0x1_0000;

/// Rendering glyph flags - 与着色器保持同步
const COLORED_FLAG: u32 = 1;
const WIDE_CHAR_FLAG: u32 = 2;

pub struct WgpuRenderer {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,

    // -- 文本渲染管线 --
    text_bg_pipeline: wgpu::RenderPipeline,
    text_fg_pipeline: wgpu::RenderPipeline,
    text_uniform_buffer: wgpu::Buffer,
    text_uniform_bind_group: wgpu::BindGroup,
    text_instance_buffer: wgpu::Buffer,
    text_index_buffer: wgpu::Buffer,
    text_texture_bind_group_layout: wgpu::BindGroupLayout,
    text_sampler: wgpu::Sampler,

    // -- 矩形渲染管线 --
    rect_pipelines: [wgpu::RenderPipeline; 4], // normal, undercurl, dotted, dashed
    rect_uniform_buffer: wgpu::Buffer,
    rect_uniform_bind_group: wgpu::BindGroup,
    rect_vertex_buffer: wgpu::Buffer,

    // -- Atlas / 字形管理 --
    atlases: Vec<Atlas>,
    atlas_bind_groups: Vec<AtlasBindGroup>,
    current_atlas: usize,
}

impl std::fmt::Debug for WgpuRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WgpuRenderer").finish_non_exhaustive()
    }
}

impl WgpuRenderer {
    pub fn new(
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
        surface_format: wgpu::TextureFormat,
    ) -> Self {
        info!("正在初始化 wgpu 渲染器 (DX12)");

        // =============================
        // 文本着色器模块
        // =============================
        let text_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("text_shader"),
            source: wgpu::ShaderSource::Wgsl(TEXT_SHADER.into()),
        });

        // =============================
        // 文本 uniform buffer + bind group layout
        // =============================
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
            bind_group_layouts: &[&text_uniform_bind_group_layout, &text_texture_bind_group_layout],
            push_constant_ranges: &[],
        });

        // 实例 buffer 的顶点布局
        let text_instance_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<TextInstanceData>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &[
                // grid_coords: col, row
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Uint32x2,
                    offset: 0,
                    shader_location: 0,
                },
                // glyph: left, top, width, height
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Sint32x4,
                    offset: 8,
                    shader_location: 1,
                },
                // uv: uv_left, uv_bot, uv_width, uv_height
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x4,
                    offset: 24,
                    shader_location: 2,
                },
                // text_color: r, g, b, cell_flags
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Uint32x4,
                    offset: 40,
                    shader_location: 3,
                },
                // bg_color: r, g, b, a
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Uint32x4,
                    offset: 56,
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
                entry_point: Some("vs_main"),
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
            multiview: None,
            cache: None,
        });

        // 文字 pass 管线 - 使用标准 alpha 混合
        let text_fg_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("text_fg_pipeline"),
            layout: Some(&text_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &text_shader,
                entry_point: Some("vs_main"),
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
            multiview: None,
            cache: None,
        });

        // =============================
        // 文本 index buffer (6 个索引构成一个四边形)
        // =============================
        let indices: [u32; 6] = [0, 1, 3, 1, 2, 3];
        let text_index_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("text_index_buffer"),
            size: (6 * std::mem::size_of::<u32>()) as u64,
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&text_index_buffer, 0, bytemuck::cast_slice(&indices));

        // =============================
        // 文本 instance buffer
        // =============================
        let text_instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("text_instance_buffer"),
            size: (BATCH_MAX * std::mem::size_of::<TextInstanceData>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

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
            bind_group_layouts: &[&rect_uniform_bind_group_layout],
            push_constant_ranges: &[],
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
                multiview: None,
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

        // =============================
        // 初始 Atlas
        // =============================
        let initial_atlas = Atlas::new(&device, ATLAS_SIZE);
        let initial_bind_group = Self::create_atlas_bind_group(
            &device,
            &text_texture_bind_group_layout,
            &initial_atlas,
            &text_sampler,
        );

        info!("wgpu 渲染器初始化完成");

        Self {
            device,
            queue,

            text_bg_pipeline,
            text_fg_pipeline,
            text_uniform_buffer,
            text_uniform_bind_group,
            text_instance_buffer,
            text_index_buffer,
            text_texture_bind_group_layout,
            text_sampler,

            rect_pipelines,
            rect_uniform_buffer,
            rect_uniform_bind_group,
            rect_vertex_buffer,

            atlases: vec![initial_atlas],
            atlas_bind_groups: vec![initial_bind_group],
            current_atlas: 0,
        }
    }

    fn create_atlas_bind_group(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        atlas: &Atlas,
        sampler: &wgpu::Sampler,
    ) -> AtlasBindGroup {
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("atlas_bind_group"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&atlas.texture_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        });
        AtlasBindGroup { bind_group }
    }

    /// 确保 atlas bind groups 与 atlases 数量一致.
    fn sync_atlas_bind_groups(&mut self) {
        while self.atlas_bind_groups.len() < self.atlases.len() {
            let idx = self.atlas_bind_groups.len();
            let bg = Self::create_atlas_bind_group(
                &self.device,
                &self.text_texture_bind_group_layout,
                &self.atlases[idx],
                &self.text_sampler,
            );
            self.atlas_bind_groups.push(bg);
        }
    }

    /// 绘制单元格 (文本和背景).
    pub fn draw_cells<I: Iterator<Item = RenderableCell>>(
        &mut self,
        size_info: &SizeInfo,
        glyph_cache: &mut GlyphCache,
        cells: I,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
    ) {
        // 收集所有实例数据, 按 atlas 分组
        let mut instances_by_atlas: std::collections::HashMap<usize, Vec<TextInstanceData>> =
            std::collections::HashMap::new();

        for cell in cells {
            self.process_cell(cell, glyph_cache, size_info, &mut instances_by_atlas);
        }

        // 更新 projection uniform
        let uniforms_bg = self.compute_text_uniforms(size_info, 0);
        let uniforms_fg = self.compute_text_uniforms(size_info, 1);

        // 确保 bind groups 同步
        self.sync_atlas_bind_groups();

        // -- 背景 pass --
        self.queue.write_buffer(&self.text_uniform_buffer, 0, bytemuck::bytes_of(&uniforms_bg));

        for (atlas_idx, instances) in &instances_by_atlas {
            if instances.is_empty() {
                continue;
            }

            // 如果实例数超出 buffer 大小, 需要分批
            for chunk in instances.chunks(BATCH_MAX) {
                self.queue.write_buffer(&self.text_instance_buffer, 0, bytemuck::cast_slice(chunk));

                // 背景 pass
                self.queue.write_buffer(
                    &self.text_uniform_buffer,
                    0,
                    bytemuck::bytes_of(&uniforms_bg),
                );

                let bind_group = &self.atlas_bind_groups[*atlas_idx];
                {
                    let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("text_bg_pass"),
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
                    });
                    rpass.set_pipeline(&self.text_bg_pipeline);
                    rpass.set_bind_group(0, &self.text_uniform_bind_group, &[]);
                    rpass.set_bind_group(1, &bind_group.bind_group, &[]);
                    rpass.set_index_buffer(
                        self.text_index_buffer.slice(..),
                        wgpu::IndexFormat::Uint32,
                    );
                    rpass.set_vertex_buffer(0, self.text_instance_buffer.slice(..));
                    rpass.draw_indexed(0..6, 0, 0..chunk.len() as u32);
                }

                // 文字 pass
                self.queue.write_buffer(
                    &self.text_uniform_buffer,
                    0,
                    bytemuck::bytes_of(&uniforms_fg),
                );

                {
                    let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("text_fg_pass"),
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
                    });
                    rpass.set_pipeline(&self.text_fg_pipeline);
                    rpass.set_bind_group(0, &self.text_uniform_bind_group, &[]);
                    rpass.set_bind_group(1, &bind_group.bind_group, &[]);
                    rpass.set_index_buffer(
                        self.text_index_buffer.slice(..),
                        wgpu::IndexFormat::Uint32,
                    );
                    rpass.set_vertex_buffer(0, self.text_instance_buffer.slice(..));
                    rpass.draw_indexed(0..6, 0, 0..chunk.len() as u32);
                }
            }
        }
    }

    fn process_cell(
        &mut self,
        mut cell: RenderableCell,
        glyph_cache: &mut GlyphCache,
        _size_info: &SizeInfo,
        instances_by_atlas: &mut std::collections::HashMap<usize, Vec<TextInstanceData>>,
    ) {
        // 获取 cell 的字体 key
        let font_key = match cell.flags & Flags::BOLD_ITALIC {
            Flags::BOLD_ITALIC => glyph_cache.bold_italic_key,
            Flags::ITALIC => glyph_cache.italic_key,
            Flags::BOLD => glyph_cache.bold_key,
            _ => glyph_cache.font_key,
        };

        // 隐藏的单元格和 tab 渲染为空格
        let hidden = cell.flags.contains(Flags::HIDDEN);
        if cell.character == '\t' || hidden {
            cell.character = ' ';
        }

        let glyph_key = crossfont::GlyphKey {
            font_key,
            size: glyph_cache.font_size,
            character: cell.character,
        };

        // 加载字形
        let mut loader = WgpuGlyphLoader {
            device: &self.device,
            queue: &self.queue,
            atlases: &mut self.atlases,
            current_atlas: &mut self.current_atlas,
        };
        let glyph = glyph_cache.get(glyph_key, &mut loader, true);
        let instance = Self::create_instance(&cell, &glyph);
        instances_by_atlas.entry(glyph.atlas_index).or_default().push(instance);

        // 渲染可见的零宽字符
        if let Some(zerowidth) =
            cell.extra.as_mut().and_then(|extra| extra.zerowidth.take().filter(|_| !hidden))
        {
            for character in zerowidth {
                let glyph_key =
                    crossfont::GlyphKey { font_key, size: glyph_cache.font_size, character };
                let glyph = glyph_cache.get(glyph_key, &mut loader, false);
                let instance = Self::create_instance(&cell, &glyph);
                instances_by_atlas.entry(glyph.atlas_index).or_default().push(instance);
            }
        }
    }

    fn create_instance(cell: &RenderableCell, glyph: &Glyph) -> TextInstanceData {
        let mut cell_flags: u32 = 0;
        if glyph.multicolor {
            cell_flags |= COLORED_FLAG;
        }
        if cell.flags.contains(Flags::WIDE_CHAR) {
            cell_flags |= WIDE_CHAR_FLAG;
        }

        TextInstanceData {
            col: cell.point.column.0 as u32,
            row: cell.point.line as u32,
            glyph_left: glyph.left as i32,
            glyph_top: glyph.top as i32,
            glyph_width: glyph.width as i32,
            glyph_height: glyph.height as i32,
            uv_left: glyph.uv_left,
            uv_bot: glyph.uv_bot,
            uv_width: glyph.uv_width,
            uv_height: glyph.uv_height,
            text_r: cell.fg.r as u32,
            text_g: cell.fg.g as u32,
            text_b: cell.fg.b as u32,
            cell_flags,
            bg_r: cell.bg.r as u32,
            bg_g: cell.bg.g as u32,
            bg_b: cell.bg.b as u32,
            bg_a: (cell.bg_alpha * 255.0) as u32,
        }
    }

    fn compute_text_uniforms(&self, size: &SizeInfo, rendering_pass: i32) -> TextUniforms {
        let width = size.width();
        let height = size.height();
        let padding_x = size.padding_x();
        let padding_y = size.padding_y();

        let scale_x = 2. / (width - 2. * padding_x);
        let scale_y = -2. / (height - 2. * padding_y);
        let offset_x = -1.;
        let offset_y = 1.;

        TextUniforms {
            projection: [offset_x, offset_y, scale_x, scale_y],
            cell_dim: [size.cell_width(), size.cell_height()],
            rendering_pass,
            _pad: 0,
        }
    }

    /// 在变化的位置绘制字符串 - 用于渲染计时器, 警告和错误.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_string(
        &mut self,
        point: Point<usize>,
        fg: Rgb,
        bg: Rgb,
        string_chars: impl Iterator<Item = char>,
        size_info: &SizeInfo,
        glyph_cache: &mut GlyphCache,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
    ) {
        let mut wide_char_spacer = false;
        let cells = string_chars.enumerate().filter_map(|(i, character)| {
            let flags = if wide_char_spacer {
                wide_char_spacer = false;
                return None;
            } else if character.width() == Some(2) {
                wide_char_spacer = true;
                Flags::WIDE_CHAR
            } else {
                Flags::empty()
            };

            Some(RenderableCell {
                point: Point::new(point.line, point.column + i),
                character,
                extra: None,
                flags,
                bg_alpha: 1.0,
                fg,
                bg,
                underline: fg,
            })
        });

        self.draw_cells(size_info, glyph_cache, cells, encoder, view);
    }

    /// 清屏
    pub fn clear(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        color: Rgb,
        alpha: f32,
    ) {
        let r = (f32::from(color.r) / 255.0).min(1.0) * alpha;
        let g = (f32::from(color.g) / 255.0).min(1.0) * alpha;
        let b = (f32::from(color.b) / 255.0).min(1.0) * alpha;

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
        });
        // render pass 在 drop 时自动结束
    }

    /// 绘制矩形
    pub fn draw_rects(
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

        let half_width = size_info.width() / 2.;
        let half_height = size_info.height() / 2.;

        // 按 rect_kind 分类顶点
        let mut vertices_by_kind: [Vec<RectVertex>; 4] = Default::default();
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
        let viewport_height = size_info.height() - size_info.padding_y();
        let padding_y = viewport_height
            - (viewport_height / size_info.cell_height()).floor() * size_info.cell_height();

        // 逆序绘制, 普通矩形在最上面
        for kind_idx in (0..4).rev() {
            let vertices = &vertices_by_kind[kind_idx];
            if vertices.is_empty() {
                continue;
            }

            let rect_uniforms = RectUniforms {
                cell_width: size_info.cell_width(),
                cell_height: size_info.cell_height(),
                padding_x: size_info.padding_x(),
                padding_y,
                underline_position,
                underline_thickness: metrics.underline_thickness,
                undercurl_position: position,
                rect_kind: kind_idx as i32,
            };

            self.queue.write_buffer(
                &self.rect_uniform_buffer,
                0,
                bytemuck::bytes_of(&rect_uniforms),
            );

            // 如果顶点数超过 buffer 大小, 需要重新创建或分批
            let vertex_data = bytemuck::cast_slice(vertices);
            let needed_size = vertex_data.len() as u64;
            if needed_size > self.rect_vertex_buffer.size() {
                // 简单方案: 重建更大的 buffer
                // 由于 self 是 &mut, 我们可以直接重建
                self.rect_vertex_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("rect_vertex_buffer_resized"),
                    size: needed_size,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
            }
            self.queue.write_buffer(&self.rect_vertex_buffer, 0, vertex_data);

            {
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
                });
                rpass.set_pipeline(&self.rect_pipelines[kind_idx]);
                rpass.set_bind_group(0, &self.rect_uniform_bind_group, &[]);
                rpass.set_vertex_buffer(0, self.rect_vertex_buffer.slice(..));
                rpass.draw(0..vertices.len() as u32, 0..1);
            }
        }
    }

    fn add_rect_vertices(
        vertices: &mut Vec<RectVertex>,
        half_width: f32,
        half_height: f32,
        rect: &RenderRect,
    ) {
        // 计算归一化设备坐标中的矩形顶点位置.
        // NDC 范围从 -1 到 +1, Y 轴向上.
        let x = rect.x / half_width - 1.0;
        let y = -rect.y / half_height + 1.0;
        let width = rect.width / half_width;
        let height = rect.height / half_height;
        let (r, g, b) = rect.color.as_tuple();
        let a = rect.alpha;
        let r = r as f32 / 255.0;
        let g = g as f32 / 255.0;
        let b = b as f32 / 255.0;

        // 两个三角形构成一个四边形
        let quad = [
            RectVertex { x, y, r, g, b, a },
            RectVertex { x, y: y - height, r, g, b, a },
            RectVertex { x: x + width, y, r, g, b, a },
            RectVertex { x: x + width, y: y - height, r, g, b, a },
        ];

        vertices.push(quad[0]);
        vertices.push(quad[1]);
        vertices.push(quad[2]);
        vertices.push(quad[2]);
        vertices.push(quad[3]);
        vertices.push(quad[1]);
    }

    /// 调整渲染器大小 (viewport 更新).
    /// wgpu 不需要显式设置 viewport, 它由 surface 配置决定.
    pub fn resize(&self, _size_info: &SizeInfo) {
        // wgpu 中 viewport 通过 surface reconfigure 处理,
        // 不需要像 OpenGL 那样的 glViewport 调用.
        // 保留此函数以保持接口兼容.
    }

    /// 获取用于 glyph 加载的 loader api.
    pub fn loader_api(&mut self) -> WgpuLoaderApi<'_> {
        WgpuLoaderApi {
            device: &self.device,
            queue: &self.queue,
            atlases: &mut self.atlases,
            current_atlas: &mut self.current_atlas,
        }
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }
}

/// 用于在渲染循环外加载字形的 API.
pub struct WgpuLoaderApi<'a> {
    device: &'a wgpu::Device,
    queue: &'a wgpu::Queue,
    atlases: &'a mut Vec<Atlas>,
    current_atlas: &'a mut usize,
}

impl LoadGlyph for WgpuLoaderApi<'_> {
    fn load_glyph(&mut self, rasterized: &RasterizedGlyph) -> Glyph {
        Atlas::load_glyph(self.device, self.queue, self.atlases, self.current_atlas, rasterized)
    }

    fn clear(&mut self) {
        Atlas::clear_atlas(self.atlases, self.current_atlas);
    }
}

/// 用于在渲染期间加载字形的内部 loader.
struct WgpuGlyphLoader<'a> {
    device: &'a wgpu::Device,
    queue: &'a wgpu::Queue,
    atlases: &'a mut Vec<Atlas>,
    current_atlas: &'a mut usize,
}

impl LoadGlyph for WgpuGlyphLoader<'_> {
    fn load_glyph(&mut self, rasterized: &RasterizedGlyph) -> Glyph {
        Atlas::load_glyph(self.device, self.queue, self.atlases, self.current_atlas, rasterized)
    }

    fn clear(&mut self) {
        Atlas::clear_atlas(self.atlases, self.current_atlas);
    }
}
