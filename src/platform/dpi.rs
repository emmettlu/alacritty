use serde::{Deserialize, Serialize};
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhysicalSize<T> {
    pub width: T,
    pub height: T,
}
impl<T> PhysicalSize<T> {
    pub const fn new(width: T, height: T) -> Self {
        Self { width, height }
    }
}
impl<T> From<(T, T)> for PhysicalSize<T> {
    fn from((width, height): (T, T)) -> Self {
        Self::new(width, height)
    }
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhysicalPosition<T> {
    pub x: T,
    pub y: T,
}
impl<T> PhysicalPosition<T> {
    pub const fn new(x: T, y: T) -> Self {
        Self { x, y }
    }
}
impl<T> From<(T, T)> for PhysicalPosition<T> {
    fn from((x, y): (T, T)) -> Self {
        Self::new(x, y)
    }
}

impl<T> From<PhysicalPosition<T>> for (T, T) {
    fn from(p: PhysicalPosition<T>) -> Self {
        (p.x, p.y)
    }
}
impl<T> From<PhysicalSize<T>> for (T, T) {
    fn from(s: PhysicalSize<T>) -> Self {
        (s.width, s.height)
    }
}
