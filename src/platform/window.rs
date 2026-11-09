pub use super::native::Window;
use super::{
    dpi::{PhysicalPosition, PhysicalSize},
    monitor::MonitorHandle,
};
use serde::{Deserialize, Serialize};
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WindowId(pub u64);
impl WindowId {
    pub const fn dummy() -> Self {
        Self(0)
    }
}
impl From<WindowId> for u64 {
    fn from(id: WindowId) -> Self {
        id.0
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorIcon {
    Default,
    Text,
    Pointer,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Light,
    Dark,
}
#[derive(Debug, Clone)]
pub enum Fullscreen {
    Borderless(Option<MonitorHandle>),
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum WindowLevel {
    #[default]
    Normal,
    AlwaysOnTop,
}
#[derive(Debug, Clone, Copy)]
pub enum UserAttentionType {
    Critical,
}
#[derive(Debug, Clone, Copy)]
pub enum ImePurpose {
    Terminal,
}
pub use crate::config::window::OptionAsAlt;
#[derive(Debug, Clone)]
pub struct WindowAttributes {
    pub title: String,
    pub position: Option<PhysicalPosition<i32>>,
    pub size: PhysicalSize<u32>,
    pub decorations: crate::config::window::Decorations,
    pub transparent: bool,
    pub blur: bool,
    pub maximized: bool,
    pub fullscreen: bool,
    pub level: WindowLevel,
    pub theme: Option<Theme>,
    pub option_as_alt: OptionAsAlt,
    pub tabbing_id: Option<String>,
    pub class: crate::config::window::Class,
    #[cfg(all(unix, not(target_os = "macos")))]
    pub embed: Option<u32>,
}
