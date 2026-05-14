// wgpu 后端的字形缓存
// 对应原 renderer/text/glyph_cache.rs, 但使用 wgpu atlas 而非 GL atlas

use std::collections::HashMap;

use ahash::RandomState;
use crossfont::{
    Error as RasterizerError, FontDesc, FontKey, GlyphKey, Metrics, Rasterize, RasterizedGlyph,
    Rasterizer, Size, Slant, Style, Weight,
};
use log::{error, info};
use unicode_width::UnicodeWidthChar;

use crate::config::font::{Font, FontDescription};
use crate::config::ui_config::Delta;

use super::builtin_font;

/// 允许将光栅化的字形复制到 GPU 内存的 trait.
pub trait LoadGlyph {
    /// 将光栅化的字形加载到 GPU 内存.
    fn load_glyph(&mut self, rasterized: &RasterizedGlyph) -> Glyph;

    /// 清除从先前加载的字形中累积的状态.
    fn clear(&mut self);
}

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

/// 简单的字形缓存.
///
/// 当前仅以 `GlyphKey` 为键, 因此无法保存同一码点的不同表示.
pub struct GlyphCache {
    /// 已缓存的字形.
    cache: HashMap<GlyphKey, Glyph, RandomState>,

    /// 用于加载新字形的光栅化器.
    rasterizer: Rasterizer,

    /// 常规字体.
    pub font_key: FontKey,

    /// 粗体字体.
    pub bold_key: FontKey,

    /// 斜体字体.
    pub italic_key: FontKey,

    /// 粗斜体字体.
    pub bold_italic_key: FontKey,

    /// 字体大小.
    pub font_size: crossfont::Size,

    /// 字体偏移.
    font_offset: Delta<i8>,

    /// 字形偏移.
    glyph_offset: Delta<i8>,

    /// 字体度量.
    metrics: Metrics,

    /// 是否使用内置字体绘制 box drawing 字符.
    builtin_box_drawing: bool,
}

impl GlyphCache {
    pub fn new(mut rasterizer: Rasterizer, font: &Font) -> Result<GlyphCache, crossfont::Error> {
        let (regular, bold, italic, bold_italic) = Self::compute_font_keys(font, &mut rasterizer)?;

        let metrics = GlyphCache::load_font_metrics(&mut rasterizer, font, regular)?;
        Ok(Self {
            cache: Default::default(),
            rasterizer,
            font_size: font.size(),
            font_key: regular,
            bold_key: bold,
            italic_key: italic,
            bold_italic_key: bold_italic,
            font_offset: font.offset,
            glyph_offset: font.glyph_offset,
            metrics,
            builtin_box_drawing: font.builtin_box_drawing,
        })
    }

    // 加载字体度量并调整字形偏移.
    fn load_font_metrics(
        rasterizer: &mut Rasterizer,
        font: &Font,
        key: FontKey,
    ) -> Result<Metrics, crossfont::Error> {
        // 在调用 metrics 之前需要至少加载一个字形.
        rasterizer.get_glyph(GlyphKey { font_key: key, character: 'm', size: font.size() })?;

        let mut metrics = rasterizer.metrics(key, font.size())?;
        metrics.strikeout_position += font.glyph_offset.y as f32;
        Ok(metrics)
    }

    fn load_glyphs_for_font<L: LoadGlyph>(&mut self, font: FontKey, loader: &mut L) {
        let size = self.font_size;

        // 缓存所有 ASCII 字符.
        for i in 32u8..=126u8 {
            self.get(GlyphKey { font_key: font, character: i as char, size }, loader, true);
        }
    }

    /// 计算字体键 (Regular, Bold, Italic, Bold Italic).
    fn compute_font_keys(
        font: &Font,
        rasterizer: &mut Rasterizer,
    ) -> Result<(FontKey, FontKey, FontKey, FontKey), crossfont::Error> {
        let size = font.size();

        // 加载常规字体.
        let regular_desc = Self::make_desc(font.normal(), Slant::Normal, Weight::Normal);

        let regular = Self::load_regular_font(rasterizer, &regular_desc, size)?;

        // 如果描述不是 regular_desc, 则加载.
        let mut load_or_regular = |desc: FontDesc| {
            if desc == regular_desc {
                regular
            } else {
                rasterizer.load_font(&desc, size).unwrap_or(regular)
            }
        };

        // 加载粗体字体.
        let bold_desc = Self::make_desc(&font.bold(), Slant::Normal, Weight::Bold);
        let bold = load_or_regular(bold_desc);

        // 加载斜体字体.
        let italic_desc = Self::make_desc(&font.italic(), Slant::Italic, Weight::Normal);
        let italic = load_or_regular(italic_desc);

        // 加载粗斜体字体.
        let bold_italic_desc = Self::make_desc(&font.bold_italic(), Slant::Italic, Weight::Bold);
        let bold_italic = load_or_regular(bold_italic_desc);

        Ok((regular, bold, italic, bold_italic))
    }

    fn load_regular_font(
        rasterizer: &mut Rasterizer,
        description: &FontDesc,
        size: Size,
    ) -> Result<FontKey, crossfont::Error> {
        match rasterizer.load_font(description, size) {
            Ok(font) => Ok(font),
            Err(err) => {
                error!("{err}");

                let fallback_desc =
                    Self::make_desc(Font::default().normal(), Slant::Normal, Weight::Normal);
                rasterizer.load_font(&fallback_desc, size)
            },
        }
    }

    fn make_desc(desc: &FontDescription, slant: Slant, weight: Weight) -> FontDesc {
        let style = if let Some(ref spec) = desc.style {
            Style::Specific(spec.to_owned())
        } else {
            Style::Description { slant, weight }
        };
        FontDesc::new(desc.family.clone(), style)
    }

    /// 从字体获取一个字形.
    ///
    /// 如果字形从未加载过, 将对其进行光栅化并插入缓存.
    pub fn get<L>(&mut self, glyph_key: GlyphKey, loader: &mut L, show_missing: bool) -> Glyph
    where
        L: LoadGlyph + ?Sized,
    {
        // 尝试从缓存中加载字形.
        if let Some(glyph) = self.cache.get(&glyph_key) {
            return *glyph;
        };

        // 使用内置字体为特殊字符光栅化, 否则使用用户字体.
        let rasterized = self
            .builtin_box_drawing
            .then(|| {
                builtin_font::builtin_glyph(
                    glyph_key.character,
                    &self.metrics,
                    &self.font_offset,
                    &self.glyph_offset,
                )
            })
            .flatten()
            .map_or_else(|| self.rasterizer.get_glyph(glyph_key), Ok);

        let glyph = match rasterized {
            Ok(rasterized) => self.load_glyph(loader, rasterized),
            // 加载缺失字形的备用字形.
            Err(RasterizerError::MissingGlyph(rasterized)) if show_missing => {
                // 使用 `\0` 作为 "missing" 字形, 只缓存一次.
                let missing_key = GlyphKey { character: '\0', ..glyph_key };
                if let Some(glyph) = self.cache.get(&missing_key) {
                    *glyph
                } else {
                    let glyph = self.load_glyph(loader, rasterized);
                    self.cache.insert(missing_key, glyph);

                    glyph
                }
            },
            Err(_) => self.load_glyph(loader, Default::default()),
        };

        // 缓存光栅化的字形.
        *self.cache.entry(glyph_key).or_insert(glyph)
    }

    /// 将字形加载到 atlas 中.
    ///
    /// 在加载之前, 将应用为字形缓存定义的所有变换.
    pub fn load_glyph<L>(&self, loader: &mut L, mut glyph: RasterizedGlyph) -> Glyph
    where
        L: LoadGlyph + ?Sized,
    {
        glyph.left += i32::from(self.glyph_offset.x);
        glyph.top += i32::from(self.glyph_offset.y);
        glyph.top -= self.metrics.descent as i32;

        // 零宽字符的度量是基于当前 cell 之后的字符渲染的,
        // 锚点在前一个字符的右侧. 由于我们在前一个字符内渲染零宽字符,
        // 锚点已向右移动了一个 cell.
        if glyph.character.width() == Some(0) {
            glyph.left += self.metrics.average_advance as i32;
        }

        // 将字形添加到缓存.
        loader.load_glyph(&glyph)
    }

    /// 将 GL 和注册表中当前缓存的数据重置为默认状态.
    pub fn reset_glyph_cache<L: LoadGlyph>(&mut self, loader: &mut L) {
        loader.clear();
        self.cache = Default::default();

        self.load_common_glyphs(loader);
    }

    /// 更新内部字体大小.
    ///
    /// 注意: 要重新加载渲染器的字体, 之后应调用 [`Self::reset_glyph_cache`].
    pub fn update_font_size(&mut self, font: &Font) -> Result<(), crossfont::Error> {
        // 更新 dpi 缩放.
        self.font_offset = font.offset;
        self.glyph_offset = font.glyph_offset;

        // 重新计算字体键.
        let (regular, bold, italic, bold_italic) =
            Self::compute_font_keys(font, &mut self.rasterizer)?;

        let metrics = GlyphCache::load_font_metrics(&mut self.rasterizer, font, regular)?;

        info!("Font size changed to {:?} px", font.size().as_px());

        self.font_size = font.size();
        self.font_key = regular;
        self.bold_key = bold;
        self.italic_key = italic;
        self.bold_italic_key = bold_italic;
        self.metrics = metrics;
        self.builtin_box_drawing = font.builtin_box_drawing;

        Ok(())
    }

    pub fn font_metrics(&self) -> crossfont::Metrics {
        self.metrics
    }

    /// 预取几乎肯定会被加载的字形.
    pub fn load_common_glyphs<L: LoadGlyph>(&mut self, loader: &mut L) {
        self.load_glyphs_for_font(self.font_key, loader);
        self.load_glyphs_for_font(self.bold_key, loader);
        self.load_glyphs_for_font(self.italic_key, loader);
        self.load_glyphs_for_font(self.bold_italic_key, loader);
    }
}
