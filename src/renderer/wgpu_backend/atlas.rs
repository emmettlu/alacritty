// wgpu 纹理 atlas - 用于缓存光栅化的字形纹理
// 对应原 renderer/text/atlas.rs 的 wgpu 版本

use std::borrow::Cow;

use crossfont::{BitmapBuffer, RasterizedGlyph};
use log::warn;

/// Atlas 纹理大小 (像素).
///
/// 初始 atlas 用 512x512 降低启动显存占用, 后续空间不足会自动创建新的 atlas.
pub const ATLAS_SIZE: u32 = 512;
const GLYPH_PADDING: u32 = 1;

#[derive(Copy, Clone, Debug)]
pub struct Glyph {
    /// 此字形所在的 atlas 索引.
    pub atlas_index: usize,
    pub multicolor: bool,
    pub top: i16,
    pub left: i16,
    pub width: i16,
    pub height: i16,
    pub uv_bot: f32,
    pub uv_left: f32,
    pub uv_width: f32,
    pub uv_height: f32,
}

impl Glyph {
    fn empty(atlas_index: usize) -> Self {
        Self {
            atlas_index,
            multicolor: false,
            top: 0,
            left: 0,
            width: 0,
            height: 0,
            uv_bot: 0.,
            uv_left: 0.,
            uv_width: 0.,
            uv_height: 0.,
        }
    }
}

/// 管理单个纹理 atlas.
///
/// 填充策略大致如下:
///
/// ```text
///                           (width, height)
///   ┌─────┬─────┬─────┬─────┬─────┐
///   │ 10  │     │     │     │     │ <- 空闲空间; 当
///   │     │     │     │     │     │    glyph_height < height - row_baseline 时可填充
///   ├─────┼─────┼─────┼─────┼─────┤
///   │ 5   │ 6   │ 7   │ 8   │ 9   │
///   │     │     │     │     │     │
///   ├─────┼─────┼─────┼─────┴─────┤ <- 行高为当前行最高字形; 作为下一行的基线
///   │ 1   │ 2   │ 3   │ 4         │
///   │     │     │     │           │ <- 当下一个字形无法放入时, 行被视为已满
///   └─────┴─────┴─────┴───────────┘
/// (0, 0)  x->
/// ```
struct Atlas {
    /// 此 atlas 的 wgpu 纹理.
    texture: wgpu::Texture,

    /// 此 atlas 纹理的 texture view.
    texture_view: wgpu::TextureView,

    /// atlas 宽度.
    width: u32,

    /// atlas 高度.
    height: u32,

    /// 当前行中最左空闲像素.
    row_extent: u32,

    /// 当前行的基线位置.
    row_baseline: u32,

    /// 当前行中最高的字形.
    row_tallest: u32,
}

/// 单个 atlas 页面及其对应的 GPU 绑定资源.
struct GlyphAtlasPage {
    atlas: Atlas,
    bind_group: wgpu::BindGroup,
}

/// 原子管理所有字形 atlas 页面及其绑定资源.
pub struct GlyphAtlas {
    device: wgpu::Device,
    queue: wgpu::Queue,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    pages: Vec<GlyphAtlasPage>,
    active_page: usize,
}

/// 插入纹理到 Atlas 时可能的错误.
enum AtlasInsertError {
    /// 纹理 atlas 已满.
    Full,

    /// 字形太大, 无法放入单个纹理.
    GlyphTooLarge,
}

impl GlyphAtlas {
    pub fn new(
        device: wgpu::Device,
        queue: wgpu::Queue,
        bind_group_layout: wgpu::BindGroupLayout,
        sampler: wgpu::Sampler,
    ) -> Self {
        let initial_page = Self::create_page(&device, &bind_group_layout, &sampler);

        Self {
            device,
            queue,
            bind_group_layout,
            sampler,
            pages: vec![initial_page],
            active_page: 0,
        }
    }

    fn create_page(
        device: &wgpu::Device,
        bind_group_layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
    ) -> GlyphAtlasPage {
        let atlas = Atlas::new(device, ATLAS_SIZE);
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("glyph_atlas_bind_group"),
            layout: bind_group_layout,
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

        GlyphAtlasPage { atlas, bind_group }
    }

    /// 加载字形到 active atlas 页面, 并在页面满时同步创建下一页及其 bind group.
    pub fn load_glyph(&mut self, rasterized: &RasterizedGlyph) -> Glyph {
        loop {
            let active_page = self.active_page;
            match self.pages[active_page]
                .atlas
                .insert(&self.queue, rasterized)
            {
                Ok(mut glyph) => {
                    glyph.atlas_index = active_page;
                    return glyph;
                }
                Err(AtlasInsertError::Full) => {
                    self.active_page += 1;
                    if self.active_page == self.pages.len() {
                        let page =
                            Self::create_page(&self.device, &self.bind_group_layout, &self.sampler);
                        self.pages.push(page);
                    }
                }
                Err(AtlasInsertError::GlyphTooLarge) => {
                    warn!(
                        "Glyph {}x{} is too large for {}x{} atlas; rendering empty glyph",
                        rasterized.width, rasterized.height, ATLAS_SIZE, ATLAS_SIZE
                    );
                    return Glyph::empty(active_page);
                }
            }
        }
    }

    /// 清除所有页面的分配状态, 并重新从第一页开始填充.
    pub fn clear(&mut self) {
        for page in &mut self.pages {
            page.atlas.clear();
        }
        self.active_page = 0;
    }

    pub(super) fn bind_group(&self, atlas_index: usize) -> &wgpu::BindGroup {
        &self.pages[atlas_index].bind_group
    }
}

impl Atlas {
    fn new(device: &wgpu::Device, size: u32) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("alacritty_glyph_atlas"),
            size: wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // 使用 RGBA8 纹理, 同时用于普通和 emoji 字形.
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        Self {
            texture,
            texture_view,
            width: size,
            height: size,
            row_extent: 0,
            row_baseline: 0,
            row_tallest: 0,
        }
    }

    fn clear(&mut self) {
        self.row_extent = 0;
        self.row_baseline = 0;
        self.row_tallest = 0;
    }

    /// 将一个 RasterizedGlyph 插入到纹理 atlas 中.
    fn insert(
        &mut self,
        queue: &wgpu::Queue,
        glyph: &RasterizedGlyph,
    ) -> Result<Glyph, AtlasInsertError> {
        let glyph_width = glyph.width as u32;
        let glyph_height = glyph.height as u32;

        let padded_width = glyph_width.saturating_add(GLYPH_PADDING);
        let padded_height = glyph_height.saturating_add(GLYPH_PADDING);
        if padded_width > self.width || padded_height > self.height {
            return Err(AtlasInsertError::GlyphTooLarge);
        }

        // 如果当前行空间不足, 换到下一行.
        if !self.room_in_row(glyph) {
            self.advance_row()?;
        }

        // 如果仍然没有空间, 则返回错误.
        if !self.room_in_row(glyph) {
            return Err(AtlasInsertError::Full);
        }

        Ok(self.insert_inner(queue, glyph))
    }

    /// 不检查空间, 直接插入字形.
    fn insert_inner(&mut self, queue: &wgpu::Queue, glyph: &RasterizedGlyph) -> Glyph {
        let offset_y = self.row_baseline;
        let offset_x = self.row_extent;
        let height = glyph.height as u32;
        let width = glyph.width as u32;

        // 将数据转换为 RGBA 格式.
        let (multicolor, rgba_buffer) = match &glyph.buffer {
            BitmapBuffer::Rgb(buffer) => {
                let mut rgba = Vec::with_capacity(buffer.len() / 3 * 4);
                for rgb in buffer.chunks_exact(3) {
                    // Atlas 使用 sRGB 纹理以保证 emoji 颜色正确, 但普通字形的 coverage
                    // 不能放在 RGB 通道里, 否则采样时会被 sRGB -> linear 转换压暗,
                    // 导致文字看起来过细. alpha 通道不会做 sRGB 转换.
                    rgba.push(u8::MAX);
                    rgba.push(u8::MAX);
                    rgba.push(u8::MAX);
                    rgba.push(rgb[0]);
                }
                (false, Cow::Owned(rgba))
            }
            BitmapBuffer::Rgba(buffer) => (true, Cow::Borrowed(buffer.as_slice())),
        };

        // 上传数据到 GPU 纹理.
        if width > 0 && height > 0 && !rgba_buffer.is_empty() {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: offset_x,
                        y: offset_y,
                        z: 0,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                &rgba_buffer,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(4 * width),
                    rows_per_image: Some(height),
                },
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
        }

        // 更新 Atlas 状态.
        let padding = u32::from(width > 0 || height > 0) * GLYPH_PADDING;
        self.row_extent = offset_x + width + padding;
        self.row_tallest = self.row_tallest.max(height + padding);

        // 生成 UV 坐标.
        let uv_bot = offset_y as f32 / self.height as f32;
        let uv_left = offset_x as f32 / self.width as f32;
        let uv_height = height as f32 / self.height as f32;
        let uv_width = width as f32 / self.width as f32;

        Glyph {
            atlas_index: 0, // 由 GlyphAtlas 设置
            multicolor,
            top: glyph.top as i16,
            left: glyph.left as i16,
            width: glyph.width as i16,
            height: glyph.height as i16,
            uv_bot,
            uv_left,
            uv_width,
            uv_height,
        }
    }

    /// 检查当前行是否有空间放置指定字形.
    fn room_in_row(&self, raw: &RasterizedGlyph) -> bool {
        let Some(remaining_height) = self.height.checked_sub(self.row_baseline) else {
            return false;
        };
        let padding = u32::from(raw.width > 0 || raw.height > 0) * GLYPH_PADDING;
        let next_extent = self
            .row_extent
            .saturating_add(raw.width as u32)
            .saturating_add(padding);
        let enough_width = next_extent <= self.width;
        let enough_height = (raw.height as u32).saturating_add(padding) <= remaining_height;

        enough_width && enough_height
    }

    /// 标记当前行已满, 准备写入下一行.
    fn advance_row(&mut self) -> Result<(), AtlasInsertError> {
        if self.row_tallest == 0 {
            return Err(AtlasInsertError::Full);
        }

        let advance_to = self.row_baseline.saturating_add(self.row_tallest);
        if advance_to >= self.height {
            return Err(AtlasInsertError::Full);
        }

        self.row_baseline = advance_to;
        self.row_extent = 0;
        self.row_tallest = 0;

        Ok(())
    }
}
