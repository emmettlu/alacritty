//! Renderer core for the Windows-only wgpu backend.

use std::fmt;

/// Shared rectangle primitives and line-shaping utilities.
pub mod rects;

/// Font fallback drawing used by glyph cache.
pub(crate) mod text;

/// Windows wgpu renderer implementation.
pub mod wgpu_backend;

/// Active glyph cache type for the renderer backend.
pub use wgpu_backend::GlyphCache;

/// Renderer-level error type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Generic renderer error.
    Other(String),
}

impl std::error::Error for Error {}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Other(err) => write!(f, "{err}"),
        }
    }
}

impl From<String> for Error {
    fn from(value: String) -> Self {
        Self::Other(value)
    }
}

impl From<&str> for Error {
    fn from(value: &str) -> Self {
        Self::Other(value.to_owned())
    }
}
