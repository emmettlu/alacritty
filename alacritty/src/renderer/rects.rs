//! Rectangle primitives and line shaping used by the renderer.
//!
//! This module is backend-agnostic and contains no OpenGL-specific code.

use std::collections::HashMap;

use ahash::RandomState;
use crossfont::Metrics;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Point};
use alacritty_terminal::term::cell::Flags;

use crate::display::SizeInfo;
use crate::display::color::Rgb;
use crate::display::content::RenderableCell;

#[derive(Debug, Copy, Clone)]
pub struct RenderRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub color: Rgb,
    pub alpha: f32,
    pub kind: RectKind,
}

impl RenderRect {
    pub fn new(x: f32, y: f32, width: f32, height: f32, color: Rgb, alpha: f32) -> Self {
        RenderRect { kind: RectKind::Normal, x, y, width, height, color, alpha }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RenderLine {
    pub start: Point<usize>,
    pub end: Point<usize>,
    pub color: Rgb,
}

#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RectKind {
    Normal = 0,
    Undercurl = 1,
    DottedUnderline = 2,
    DashedUnderline = 3,
    #[allow(dead_code)]
    NumKinds = 4,
}

impl RenderLine {
    pub fn rects(&self, flag: Flags, metrics: &Metrics, size: &SizeInfo) -> Vec<RenderRect> {
        let mut rects = Vec::new();

        let mut start = self.start;
        while start.line < self.end.line {
            let end = Point::new(start.line, size.last_column());
            Self::push_rects(&mut rects, metrics, size, flag, start, end, self.color);
            start = Point::new(start.line + 1, Column(0));
        }
        Self::push_rects(&mut rects, metrics, size, flag, start, self.end, self.color);

        rects
    }

    /// Push all rects required to draw the cell's line.
    fn push_rects(
        rects: &mut Vec<RenderRect>,
        metrics: &Metrics,
        size: &SizeInfo,
        flag: Flags,
        start: Point<usize>,
        end: Point<usize>,
        color: Rgb,
    ) {
        let (position, thickness, ty) = match flag {
            Flags::DOUBLE_UNDERLINE => {
                // Position underlines so each one has 50% of descent available.
                let top_pos = 0.25 * metrics.descent;
                let bottom_pos = 0.75 * metrics.descent;

                rects.push(Self::create_rect(
                    size,
                    metrics.descent,
                    start,
                    end,
                    top_pos,
                    metrics.underline_thickness,
                    color,
                ));

                (bottom_pos, metrics.underline_thickness, RectKind::Normal)
            },
            // Make undercurl occupy the entire descent area.
            Flags::UNDERCURL => (metrics.descent, metrics.descent.abs(), RectKind::Undercurl),
            Flags::UNDERLINE => {
                (metrics.underline_position, metrics.underline_thickness, RectKind::Normal)
            },
            // Make dotted occupy the entire descent area.
            Flags::DOTTED_UNDERLINE => {
                (metrics.descent, metrics.descent.abs(), RectKind::DottedUnderline)
            },
            Flags::DASHED_UNDERLINE => {
                (metrics.underline_position, metrics.underline_thickness, RectKind::DashedUnderline)
            },
            Flags::STRIKEOUT => {
                (metrics.strikeout_position, metrics.strikeout_thickness, RectKind::Normal)
            },
            _ => unimplemented!("Invalid flag for cell line drawing specified"),
        };

        let mut rect =
            Self::create_rect(size, metrics.descent, start, end, position, thickness, color);
        rect.kind = ty;
        rects.push(rect);
    }

    /// Create a line's rect at a position relative to the baseline.
    fn create_rect(
        size: &SizeInfo,
        descent: f32,
        start: Point<usize>,
        end: Point<usize>,
        position: f32,
        mut thickness: f32,
        color: Rgb,
    ) -> RenderRect {
        let start_x = start.column.0 as f32 * size.cell_width();
        let end_x = (end.column.0 + 1) as f32 * size.cell_width();
        let width = end_x - start_x;

        // Make sure lines are always visible.
        thickness = thickness.max(1.);

        let line_bottom = (start.line as f32 + 1.) * size.cell_height();
        let baseline = line_bottom + descent;

        let mut y = (baseline - position - thickness / 2.).round();
        let max_y = line_bottom - thickness;
        if y > max_y {
            y = max_y;
        }

        RenderRect::new(
            start_x + size.padding_x(),
            y + size.padding_y(),
            width,
            thickness,
            color,
            1.,
        )
    }
}

/// Lines for underline and strikeout.
#[derive(Default)]
pub struct RenderLines {
    inner: HashMap<Flags, Vec<RenderLine>, RandomState>,
}

impl RenderLines {
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn rects(&self, metrics: &Metrics, size: &SizeInfo) -> Vec<RenderRect> {
        self.inner
            .iter()
            .flat_map(|(flag, lines)| {
                lines.iter().flat_map(move |line| line.rects(*flag, metrics, size))
            })
            .collect()
    }

    /// Update the stored lines with the next cell info.
    #[inline]
    pub fn update(&mut self, cell: &RenderableCell) {
        self.update_flag(cell, Flags::UNDERLINE);
        self.update_flag(cell, Flags::DOUBLE_UNDERLINE);
        self.update_flag(cell, Flags::STRIKEOUT);
        self.update_flag(cell, Flags::UNDERCURL);
        self.update_flag(cell, Flags::DOTTED_UNDERLINE);
        self.update_flag(cell, Flags::DASHED_UNDERLINE);
    }

    /// Update the lines for a specific flag.
    fn update_flag(&mut self, cell: &RenderableCell, flag: Flags) {
        if !cell.flags.contains(flag) {
            return;
        }

        // The underline color escape does not apply to strikeout.
        let color = if flag.contains(Flags::STRIKEOUT) { cell.fg } else { cell.underline };

        // Include wide char spacer if the current cell is a wide char.
        let mut end = cell.point;
        if cell.flags.contains(Flags::WIDE_CHAR) {
            end.column += 1;
        }

        // Check if there's an active line.
        if let Some(line) = self.inner.get_mut(&flag).and_then(|lines| lines.last_mut()) {
            if color == line.color
                && cell.point.column == line.end.column + 1
                && cell.point.line == line.end.line
            {
                // Update the length of the line.
                line.end = end;
                return;
            }
        }

        // Start new line if there currently is none.
        let line = RenderLine { start: cell.point, end, color };
        match self.inner.get_mut(&flag) {
            Some(lines) => lines.push(line),
            None => {
                self.inner.insert(flag, vec![line]);
            },
        }
    }
}
