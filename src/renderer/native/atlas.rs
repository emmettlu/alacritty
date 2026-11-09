//! CPU glyph atlas with coalesced upload ranges.
use std::ops::Range;

use crate::font::{BitmapBuffer, RasterizedGlyph};
use log::warn;

/// Atlas 纹理大小 (像素).
///
/// 初始 atlas 用 512x512 降低启动显存占用, 后续空间不足会自动创建新的 atlas.
const ATLAS_SIZE: u32 = 512;
const GLYPH_PADDING: u32 = 1;

#[derive(Copy, Clone, Debug)]
pub(super) struct Glyph {
    /// 此字形所在的 atlas 索引.
    pub(super) atlas_index: usize,
    pub(super) multicolor: bool,
    pub(super) top: i16,
    pub(super) left: i16,
    pub(super) width: i16,
    pub(super) height: i16,
    pub(super) uv_bot: f32,
    pub(super) uv_left: f32,
    pub(super) uv_width: f32,
    pub(super) uv_height: f32,
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
pub(super) struct Atlas {
    pub(super) pixels: Vec<u8>,
    pub(super) dirty: Option<Range<u32>>,
    pub(super) width: u32,
    pub(super) height: u32,
    row_extent: u32,
    row_baseline: u32,
    row_tallest: u32,
}
pub(super) struct GlyphAtlas {
    pub(super) pages: Vec<Atlas>,
    active_page: usize,
    rgba_scratch: Vec<u8>,
}
/// 插入纹理到 Atlas 时可能的错误.
enum AtlasInsertError {
    /// 纹理 atlas 已满.
    Full,

    /// 字形太大, 无法放入单个纹理.
    GlyphTooLarge,
}

impl GlyphAtlas {
    pub(super) fn new() -> Self {
        Self {
            pages: vec![Atlas::new(ATLAS_SIZE)],
            active_page: 0,
            rgba_scratch: Vec::new(),
        }
    }
    /// 加载字形到 active atlas 页面, 并在页面满时同步创建下一页及其 bind group.
    pub(super) fn load_glyph(&mut self, rasterized: &RasterizedGlyph) -> Glyph {
        loop {
            let active_page = self.active_page;
            let atlas = &mut self.pages[active_page];
            match atlas.insert(rasterized, &mut self.rgba_scratch) {
                Ok(mut glyph) => {
                    glyph.atlas_index = active_page;
                    return glyph;
                }
                Err(AtlasInsertError::Full) => {
                    self.active_page += 1;
                    if self.active_page == self.pages.len() {
                        let page = Atlas::new(ATLAS_SIZE);
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

    /// 清除分配状态并释放历史高水位留下的多余页面.
    pub(super) fn clear(&mut self) {
        self.pages.truncate(1);
        self.pages[0].clear();
        self.active_page = 0;
    }
}
impl Atlas {
    fn new(size: u32) -> Self {
        Self {
            pixels: vec![0; (size * size * 4) as usize],
            dirty: Some(0..size),
            width: size,
            height: size,
            row_extent: 0,
            row_baseline: 0,
            row_tallest: 0,
        }
    }
    fn clear(&mut self) {
        self.pixels.fill(0);
        self.dirty = Some(0..self.height);
        self.row_extent = 0;
        self.row_baseline = 0;
        self.row_tallest = 0;
    }

    /// 将一个 RasterizedGlyph 插入到纹理 atlas 中.
    fn insert(
        &mut self,
        glyph: &RasterizedGlyph,
        rgba_scratch: &mut Vec<u8>,
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

        Ok(self.insert_inner(glyph, rgba_scratch))
    }

    /// 不检查空间, 直接插入字形.
    fn insert_inner(&mut self, glyph: &RasterizedGlyph, rgba_scratch: &mut Vec<u8>) -> Glyph {
        let offset_y = self.row_baseline;
        let offset_x = self.row_extent;
        let height = glyph.height as u32;
        let width = glyph.width as u32;

        // 将数据转换为 RGBA 格式.
        let (multicolor, rgba_buffer) = match &glyph.buffer {
            BitmapBuffer::Rgb(buffer) => {
                rgba_scratch.clear();
                rgba_scratch.reserve(buffer.len() / 3 * 4);
                for rgb in buffer.chunks_exact(3) {
                    // Atlas 使用 sRGB 纹理以保证 emoji 颜色正确, 但普通字形的 coverage
                    // 不能放在 RGB 通道里, 否则采样时会被 sRGB -> linear 转换压暗,
                    // 导致文字看起来过细. alpha 通道不会做 sRGB 转换.
                    rgba_scratch.extend_from_slice(&[u8::MAX, u8::MAX, u8::MAX, rgb[0]]);
                }
                (false, rgba_scratch.as_slice())
            }
            BitmapBuffer::Rgba(buffer) => (true, buffer.as_slice()),
        };

        if width > 0 && height > 0 && !rgba_buffer.is_empty() {
            for row in 0..height as usize {
                let dst =
                    (((offset_y as usize + row) * self.width as usize) + offset_x as usize) * 4;
                let src = row * width as usize * 4;
                self.pixels[dst..dst + width as usize * 4]
                    .copy_from_slice(&rgba_buffer[src..src + width as usize * 4]);
            }
            let rows = offset_y..offset_y + height;
            self.dirty = Some(match self.dirty.take() {
                Some(old) => old.start.min(rows.start)..old.end.max(rows.end),
                None => rows,
            });
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
        self.pixels.fill(0);
        self.dirty = Some(0..self.height);
        self.row_extent = 0;
        self.row_tallest = 0;

        Ok(())
    }
}
