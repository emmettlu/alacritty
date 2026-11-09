use super::{
    application::ApplicationHandler,
    event::{StartCause, WindowEvent},
    native,
    window::{Window, WindowAttributes, WindowId},
};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::marker::PhantomData;
use std::sync::{Arc, mpsc};
use std::time::Instant;

thread_local! { static EVENTS: RefCell<VecDeque<(WindowId, WindowEvent)>> = const { RefCell::new(VecDeque::new()) }; }
pub(crate) fn emit(id: WindowId, event: WindowEvent) {
    EVENTS.with_borrow_mut(|queue| queue.push_back((id, event)));
}
#[derive(Debug, Clone, Copy, Default)]
pub enum ControlFlow {
    Poll,
    #[default]
    Wait,
    WaitUntil(Instant),
}
#[derive(Debug)]
pub enum DeviceEvents {
    Never,
}
pub struct ActiveEventLoop {
    exit: Cell<bool>,
    flow: Cell<ControlFlow>,
}
impl ActiveEventLoop {
    pub fn exit(&self) {
        self.exit.set(true);
        native::wake();
    }
    pub fn set_control_flow(&self, flow: ControlFlow) {
        self.flow.set(flow);
    }
    pub fn create_window(&self, attrs: WindowAttributes) -> std::io::Result<Window> {
        Window::new(attrs)
    }
    #[cfg(target_os = "macos")]
    pub fn hide_application(&self) {
        native::hide(false);
    }
    #[cfg(target_os = "macos")]
    pub fn hide_other_applications(&self) {
        native::hide(true);
    }
}
struct ProxyShared<T> {
    sender: mpsc::Sender<T>,
    alive: std::sync::atomic::AtomicBool,
    wake_pending: std::sync::atomic::AtomicBool,
}
pub struct EventLoopProxy<T> {
    shared: Arc<ProxyShared<T>>,
}
impl<T> Clone for EventLoopProxy<T> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
        }
    }
}
impl<T> std::fmt::Debug for EventLoopProxy<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EventLoopProxy")
    }
}
impl<T> EventLoopProxy<T> {
    pub fn send_event(&self, event: T) -> Result<(), mpsc::SendError<T>> {
        if !self.shared.alive.load(std::sync::atomic::Ordering::Acquire) {
            return Err(mpsc::SendError(event));
        }
        self.shared.sender.send(event)?;
        if !self
            .shared
            .wake_pending
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            native::wake();
        }
        Ok(())
    }
}
pub struct EventLoop<T> {
    active: ActiveEventLoop,
    proxy: EventLoopProxy<T>,
    receiver: mpsc::Receiver<T>,
}
pub struct EventLoopBuilder<T>(PhantomData<T>);
impl<T> EventLoopBuilder<T> {
    pub fn build(self) -> std::io::Result<EventLoop<T>> {
        native::initialize()?;
        let (sender, receiver) = mpsc::channel();
        Ok(EventLoop {
            active: ActiveEventLoop {
                exit: Cell::new(false),
                flow: Cell::new(ControlFlow::Wait),
            },
            proxy: EventLoopProxy {
                shared: Arc::new(ProxyShared {
                    sender,
                    alive: std::sync::atomic::AtomicBool::new(true),
                    wake_pending: std::sync::atomic::AtomicBool::new(false),
                }),
            },
            receiver,
        })
    }
}
impl<T> EventLoop<T> {
    pub fn with_user_event() -> EventLoopBuilder<T> {
        EventLoopBuilder(PhantomData)
    }
    pub fn create_proxy(&self) -> EventLoopProxy<T> {
        self.proxy.clone()
    }
    pub fn listen_device_events(&self, _: DeviceEvents) {}
    pub fn run_app(self, app: &mut impl ApplicationHandler<T>) -> std::io::Result<()> {
        app.new_events(&self.active, StartCause::Init);
        app.resumed(&self.active);
        while !self.active.exit.get() {
            native::drain_events();
            self.proxy
                .shared
                .wake_pending
                .store(false, std::sync::atomic::Ordering::Release);
            while let Ok(event) = self.receiver.try_recv() {
                app.user_event(&self.active, event);
            }
            loop {
                let event = EVENTS.with_borrow_mut(VecDeque::pop_front);
                let Some((id, event)) = event else { break };
                app.window_event(&self.active, id, event);
            }
            app.about_to_wait(&self.active);
            if self.active.exit.get() {
                break;
            }
            let pending = EVENTS.with_borrow(|queue| !queue.is_empty());
            if pending {
                continue;
            }
            native::wait(self.active.flow.get());
            app.new_events(&self.active, StartCause::WaitCancelled);
        }
        app.exiting(&self.active);
        self.proxy
            .shared
            .alive
            .store(false, std::sync::atomic::Ordering::Release);
        Ok(())
    }
}
