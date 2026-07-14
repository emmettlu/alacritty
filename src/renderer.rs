//! Renderer core for the wgpu backend.

use std::fmt;

/// Shared rectangle primitives and line-shaping utilities.
pub mod rects;

/// Font fallback drawing used by glyph cache.
pub(crate) mod text;

/// Windows wgpu renderer implementation.
pub mod wgpu_backend;

/// Active glyph cache type for the renderer backend.
pub use wgpu_backend::GlyphCache;

/// Renderer initialization error.
#[derive(Debug)]
pub enum Error {
    CreateSurface(wgpu::CreateSurfaceError),
    RequestAdapter(wgpu::RequestAdapterError),
    RequestDevice(wgpu::RequestDeviceError),
    MissingSurfaceCapability(&'static str),
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CreateSurface(err) => Some(err),
            Self::RequestAdapter(err) => Some(err),
            Self::RequestDevice(err) => Some(err),
            Self::MissingSurfaceCapability(_) => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateSurface(err) => write!(f, "failed to create surface: {err}"),
            Self::RequestAdapter(err) => write!(f, "failed to request adapter: {err}"),
            Self::RequestDevice(err) => write!(f, "failed to request device: {err}"),
            Self::MissingSurfaceCapability(capability) => {
                write!(f, "surface exposes no {capability}")
            }
        }
    }
}
