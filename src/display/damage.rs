//! Damage tracking for renderer updates (Windows-only path).

use std::{cmp, mem};

use crate::terminal::index::Point;
use crate::terminal::selection::SelectionRange;
use crate::terminal::term::LineDamageBounds;

use crate::display::SizeInfo;

/// Internal rectangle type used for damage regions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// State of the damage tracking for the [`Display`].
///
/// [`Display`]: crate::display::Display
#[derive(Debug)]
pub struct DamageTracker {
    /// Position of the previously drawn Vi cursor.
    pub old_vi_cursor: Option<Point<usize>>,
    /// The location of the old selection.
    pub old_selection: Option<SelectionRange>,
    /// Highlight damage submitted for debugging.
    pub debug: bool,

    /// The damage for the frames.
    frames: [FrameDamage; 2],
    screen_lines: usize,
    columns: usize,
}

impl DamageTracker {
    pub fn new(screen_lines: usize, columns: usize) -> Self {
        let mut tracker = Self {
            columns,
            screen_lines,
            debug: false,
            old_vi_cursor: None,
            old_selection: None,
            frames: Default::default(),
        };
        tracker.resize(screen_lines, columns);
        tracker
    }

    #[inline]
    #[must_use]
    pub fn frame(&mut self) -> &mut FrameDamage {
        &mut self.frames[0]
    }

    #[inline]
    #[must_use]
    pub fn next_frame(&mut self) -> &mut FrameDamage {
        &mut self.frames[1]
    }

    /// Advance to the next frame resetting the state for the active frame.
    #[inline]
    pub fn swap_damage(&mut self) {
        let screen_lines = self.screen_lines;
        let columns = self.columns;
        self.frame().reset(screen_lines, columns);
        self.frames.swap(0, 1);
    }

    /// Resize the damage information in the tracker.
    pub fn resize(&mut self, screen_lines: usize, columns: usize) {
        self.screen_lines = screen_lines;
        self.columns = columns;
        for frame in &mut self.frames {
            frame.reset(screen_lines, columns);
        }
        self.frame().full = true;
    }

    /// Damage vi cursor inside the viewport.
    pub fn damage_vi_cursor(&mut self, mut vi_cursor: Option<Point<usize>>) {
        mem::swap(&mut self.old_vi_cursor, &mut vi_cursor);

        if self.frame().full {
            return;
        }

        if let Some(vi_cursor) = self.old_vi_cursor {
            self.frame().damage_point(vi_cursor);
        }

        if let Some(vi_cursor) = vi_cursor {
            self.frame().damage_point(vi_cursor);
        }
    }

    /// Add the current frame's selection damage.
    pub fn damage_selection(
        &mut self,
        mut selection: Option<SelectionRange>,
        display_offset: usize,
    ) {
        mem::swap(&mut self.old_selection, &mut selection);

        if self.frame().full || selection == self.old_selection {
            return;
        }

        for selection in self.old_selection.into_iter().chain(selection) {
            let display_offset = display_offset as i32;
            let last_visible_line = self.screen_lines as i32 - 1;
            let columns = self.columns;

            // Ignore invisible selection.
            if selection.end.line.0 + display_offset < 0
                || selection.start.line.0.abs() < display_offset - last_visible_line
            {
                continue;
            };

            let start = cmp::max(selection.start.line.0 + display_offset, 0) as usize;
            let end = (selection.end.line.0 + display_offset).clamp(0, last_visible_line) as usize;
            for line in start..=end {
                self.frame().lines[line].expand(0, columns - 1);
            }
        }
    }
}

/// Damage state for the rendering frame.
#[derive(Debug, Default)]
pub struct FrameDamage {
    /// The entire frame needs to be redrawn.
    pub(crate) full: bool,
    /// Terminal lines damaged in the given frame.
    pub(crate) lines: Vec<LineDamageBounds>,
    /// Rectangular regions damaged in the given frame.
    pub(crate) rects: Vec<Rect>,
}

impl FrameDamage {
    /// Damage line for the given frame.
    #[inline]
    pub fn damage_line(&mut self, damage: LineDamageBounds) {
        self.lines[damage.line].expand(damage.left, damage.right);
    }

    #[inline]
    pub fn damage_point(&mut self, point: Point<usize>) {
        self.lines[point.line].expand(point.column.0, point.column.0);
    }

    /// Mark the frame as fully damaged.
    #[inline]
    pub fn mark_fully_damaged(&mut self) {
        self.full = true;
    }

    /// Add viewport rectangle to damage.
    ///
    /// This allows covering elements outside of the terminal viewport, like message bar.
    #[inline]
    pub fn add_viewport_rect(
        &mut self,
        size_info: &SizeInfo,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) {
        let y = viewport_y_to_damage_y(size_info, y, height);
        self.rects.push(Rect { x, y, width, height });
    }

    fn reset(&mut self, num_lines: usize, num_cols: usize) {
        self.full = false;
        self.rects.clear();
        self.lines.clear();
        self.lines.reserve(num_lines);
        for line in 0..num_lines {
            self.lines.push(LineDamageBounds::undamaged(line, num_cols));
        }
    }

    /// Check if a range is damaged.
    #[inline]
    pub fn intersects(&self, start: Point<usize>, end: Point<usize>) -> bool {
        let start_line = &self.lines[start.line];
        let end_line = &self.lines[end.line];
        self.full
            || (start_line.left..=start_line.right).contains(&start.column)
            || (end_line.left..=end_line.right).contains(&end.column)
            || (start.line + 1..end.line).any(|line| self.lines[line].is_damaged())
    }
}

/// Convert viewport `y` coordinate to [`Rect`] damage coordinate.
pub fn viewport_y_to_damage_y(size_info: &SizeInfo, y: i32, height: i32) -> i32 {
    size_info.height() as i32 - y - height
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_viewport_damage() {
        let mut frame_damage = FrameDamage::default();
        let viewport_height = 100.;
        let x = 0;
        let y = 40;
        let height = 5;
        let width = 10;
        let size_info = SizeInfo::new(viewport_height, viewport_height, 5., 5., 0., 0., true);
        frame_damage.add_viewport_rect(&size_info, x, y, width, height);
        assert_eq!(
            frame_damage.rects[0],
            Rect { x, y: viewport_height as i32 - y - height, width, height }
        );
        assert_eq!(frame_damage.rects[0].y, viewport_y_to_damage_y(&size_info, y, height));
    }
}
