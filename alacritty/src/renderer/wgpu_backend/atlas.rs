// wgpu 纹理 atlas - 用于缓存光栅化的字形纹理
// 对应原 renderer/text/atlas.rs 的 wgpu 版本

use std::borrow::Cow;

use crossfont::{BitmapBuffer, RasterizedGlyph};

use super::glyph_cache::Glyph;

/// Atlas 纹理大小 (像素).
pub const ATLAS_SIZE: u32 = 1024;

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
pub struct Atlas {
    /// 此 atlas 的 wgpu 纹理.
    pub texture: wgpu::Texture,

    /// 此 atlas 纹理的 texture view.
    pub texture_view: wgpu::TextureView,

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

/// 插入纹理到 Atlas 时可能的错误.
pub enum AtlasInsertError {
    /// 纹理 atlas 已满.
    Full,

    /// 字形太大, 无法放入单个纹理.
    GlyphTooLarge,
}

impl Atlas {
    pub fn new(device: &wgpu::Device, size: u32) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("alacritty_glyph_atlas"),
            size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
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

    pub fn clear(&mut self) {
        self.row_extent = 0;
        self.row_baseline = 0;
        self.row_tallest = 0;
    }

    /// 将一个 RasterizedGlyph 插入到纹理 atlas 中.
    pub fn insert(
        &mut self,
        queue: &wgpu::Queue,
        glyph: &RasterizedGlyph,
    ) -> Result<Glyph, AtlasInsertError> {
        let glyph_width = glyph.width as u32;
        let glyph_height = glyph.height as u32;

        if glyph_width > self.width || glyph_height > self.height {
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
                    rgba.push(rgb[0]);
                    rgba.push(rgb[1]);
                    rgba.push(rgb[2]);
                    rgba.push(u8::MAX);
                }
                (false, Cow::Owned(rgba))
            },
            BitmapBuffer::Rgba(buffer) => (true, Cow::Borrowed(buffer.as_slice())),
        };

        // 上传数据到 GPU 纹理.
        if width > 0 && height > 0 && !rgba_buffer.is_empty() {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: offset_x, y: offset_y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                &rgba_buffer,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(4 * width),
                    rows_per_image: Some(height),
                },
                wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            );
        }

        // 更新 Atlas 状态.
        self.row_extent = offset_x + width;
        if height > self.row_tallest {
            self.row_tallest = height;
        }

        // 生成 UV 坐标.
        let uv_bot = offset_y as f32 / self.height as f32;
        let uv_left = offset_x as f32 / self.width as f32;
        let uv_height = height as f32 / self.height as f32;
        let uv_width = width as f32 / self.width as f32;

        Glyph {
            atlas_index: 0, // 由调用方设置
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
    pub fn room_in_row(&self, raw: &RasterizedGlyph) -> bool {
        let next_extent = self.row_extent + raw.width as u32;
        let enough_width = next_extent <= self.width;
        let enough_height = (raw.height as u32) < (self.height - self.row_baseline);

        enough_width && enough_height
    }

    /// 标记当前行已满, 准备写入下一行.
    pub fn advance_row(&mut self) -> Result<(), AtlasInsertError> {
        let advance_to = self.row_baseline + self.row_tallest;
        if advance_to >= self.height {
            return Err(AtlasInsertError::Full);
        }

        self.row_baseline = advance_to;
        self.row_extent = 0;
        self.row_tallest = 0;

        Ok(())
    }

    /// 加载字形到纹理 atlas.
    ///
    /// 如果当前 atlas 已满, 将创建新的 atlas.
    #[inline]
    pub fn load_glyph(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        atlas: &mut Vec<Atlas>,
        current_atlas: &mut usize,
        rasterized: &RasterizedGlyph,
    ) -> Glyph {
        match atlas[*current_atlas].insert(queue, rasterized) {
            Ok(mut glyph) => {
                glyph.atlas_index = *current_atlas;
                glyph
            },
            Err(AtlasInsertError::Full) => {
                *current_atlas += 1;
                if *current_atlas == atlas.len() {
                    let new = Atlas::new(device, ATLAS_SIZE);
                    atlas.push(new);
                }
                Atlas::load_glyph(device, queue, atlas, current_atlas, rasterized)
            },
            Err(AtlasInsertError::GlyphTooLarge) => Glyph {
                atlas_index: *current_atlas,
                multicolor: false,
                top: 0,
                left: 0,
                width: 0,
                height: 0,
                uv_bot: 0.,
                uv_left: 0.,
                uv_width: 0.,
                uv_height: 0.,
            },
        }
    }

    #[inline]
    pub fn clear_atlas(atlas: &mut [Atlas], current_atlas: &mut usize) {
        for a in atlas.iter_mut() {
            a.clear();
        }
        *current_atlas = 0;
    }
}
