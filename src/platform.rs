//! Native windowing, input and event dispatch.
#![allow(dead_code)]

pub mod dpi;
pub mod event;
pub mod event_loop;
pub mod keyboard;
pub mod window;
pub mod monitor {
    #[derive(Debug, Clone)]
    pub struct MonitorHandle { pub refresh_rate: u32 }
    impl MonitorHandle {
        pub fn refresh_rate_millihertz(&self) -> Option<u32> { Some(self.refresh_rate) }
    }
}
pub mod application {
    use super::{event::{StartCause, WindowEvent}, event_loop::ActiveEventLoop, window::WindowId};
    pub trait ApplicationHandler<T> {
        fn resumed(&mut self, _: &ActiveEventLoop) {}
        fn new_events(&mut self, _: &ActiveEventLoop, _: StartCause) {}
        fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
        fn user_event(&mut self, _: &ActiveEventLoop, _: T) {}
        fn about_to_wait(&mut self, _: &ActiveEventLoop) {}
        fn exiting(&mut self, _: &ActiveEventLoop) {}
    }
}
#[cfg(target_os = "macos")]
#[path = "platform/macos.rs"]
pub mod native;
#[cfg(windows)]
#[path = "platform/windows.rs"]
pub mod native;
#[cfg(all(unix, not(target_os = "macos")))]
#[path = "platform/linux.rs"]
pub mod native;
