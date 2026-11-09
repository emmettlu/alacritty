//! Shared glyph batching for the native GPU backends.
use super::Error;
use super::rects::{RectKind, RenderRect};
use crate::display::SizeInfo;
use crate::display::color::{Rgb, srgb_byte_to_linear};
use crate::display::content::RenderableCell;
use crate::font::Metrics;
use crate::platform::window::Window;
use crate::terminal::index::Point;
use crate::terminal::term::cell::Flags;
use std::ops::Range;
use std::rc::Rc;
use std::sync::LazyLock;
use unicode_width::UnicodeWidthChar;
mod atlas;
mod builtin_font;
mod glyph_cache;
use atlas::{Glyph, GlyphAtlas};
pub(crate) use glyph_cache::GlyphCache;
#[cfg(target_os = "macos")]
mod metal;
#[cfg(target_os = "macos")]
use metal::Gpu;
#[cfg(windows)]
mod dx12;
#[cfg(windows)]
use dx12::Gpu;
#[cfg(all(unix, not(target_os = "macos")))]
mod vulkan;
#[cfg(all(unix, not(target_os = "macos")))]
use vulkan::Gpu;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FrameUnavailable {
    Suspended,
}
pub(crate) struct NativeFrame;
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

pub(crate) struct NativeRenderer {
    gpu: Gpu,
    window: Rc<Window>,
    opacity: f32,
    text_batches: [Vec<TextBatch>; 2],
    text_batch_counts: [usize; 2],
    text_instances: Vec<TextInstanceData>,
    text_draw_commands: [Vec<TextDrawCommand>; 2],
    rect_instances: Vec<RectInstanceData>,
    glyph_atlas: GlyphAtlas,
    uniforms: FrameUniforms,
    viewport: [f64; 4],
    clear: [f64; 4],
}
impl std::fmt::Debug for NativeRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativeRenderer")
    }
}
impl NativeRenderer {
    pub(crate) fn new(window: Rc<Window>, opacity: f32) -> Result<Self, Error> {
        let gpu = Gpu::new(window.clone(), opacity)?;
        Ok(Self {
            gpu,
            window,
            opacity,
            text_batches: Default::default(),
            text_batch_counts: [0; 2],
            text_instances: Vec::new(),
            text_draw_commands: Default::default(),
            rect_instances: Vec::new(),
            glyph_atlas: GlyphAtlas::new(),
            uniforms: bytemuck::Zeroable::zeroed(),
            viewport: [0.; 4],
            clear: [0.; 4],
        })
    }
    pub(crate) fn begin_frame(&mut self) -> Result<NativeFrame, FrameUnavailable> {
        self.text_batch_counts = [0; 2];
        let size = self.window.inner_size();
        if size.width == 0 || size.height == 0 {
            Err(FrameUnavailable::Suspended)
        } else {
            Ok(NativeFrame)
        }
    }
    pub(crate) fn render_frame(
        &mut self,
        _: &mut NativeFrame,
        size: &SizeInfo,
        metrics: &Metrics,
        rects: &[RenderRect],
        color: Rgb,
        alpha: f32,
    ) {
        self.flatten_text_batches();
        self.prepare_rects(rects);
        let (x, y, w, h) = Self::content_viewport(size);
        self.viewport = [x as f64, y as f64, w as f64, h as f64];
        self.uniforms = FrameUniforms {
            projection: [-1., 1., 2. / w, -2. / h],
            cell_dim: [size.cell_width(), size.cell_height()],
            padding: [x, y],
            underline_position: metrics.descent.abs() - metrics.underline_position.abs(),
            underline_thickness: metrics.underline_thickness,
            undercurl_position: (0.5 * metrics.descent).abs(),
            _pad: 0.,
        };
        self.clear = Self::clear_color(color, alpha);
        self.opacity = alpha;
    }
    pub(crate) fn submit_frame(&mut self, _: NativeFrame) {
        if let Err(err) = self.gpu.render(
            &self.window,
            &self.uniforms,
            self.viewport,
            &self.text_instances,
            &self.text_draw_commands,
            &self.rect_instances,
            &mut self.glyph_atlas,
            self.clear,
        ) {
            log::warn!("Native frame unavailable: {err}");
            self.window.request_redraw();
        }
    }
    pub(crate) fn present_clear(&mut self, color: Rgb, alpha: f32) {
        let size = self.window.inner_size();
        self.viewport = [0., 0., size.width as f64, size.height as f64];
        self.clear = Self::clear_color(color, alpha);
        self.submit_frame(NativeFrame);
    }
    fn clear_color(color: Rgb, alpha: f32) -> [f64; 4] {
        [
            srgb_to_linear_f32(color.r) as f64 * alpha as f64,
            srgb_to_linear_f32(color.g) as f64 * alpha as f64,
            srgb_to_linear_f32(color.b) as f64 * alpha as f64,
            alpha as f64,
        ]
    }
    fn content_viewport(size_info: &SizeInfo) -> (f32, f32, f32, f32) {
        let x = size_info.padding_x();
        let y = size_info.padding_y();
        let width = (size_info.width() - 2.0 * x).max(1.0);
        let height = (size_info.height() - 2.0 * y).max(1.0);
        (x, y, width, height)
    }

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
