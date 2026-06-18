//! Text rendering support for the Windows-only wgpu path.
//!
//! OpenGL text renderers were removed. This module now only exposes
//! built-in glyph rasterization helpers used by the wgpu glyph cache.

pub(crate) mod builtin_font;
