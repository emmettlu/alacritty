//! Font rasterization shared by the renderer.
#[cfg(not(target_os = "macos"))]
pub use crossfont::*;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;
