use super::{
    dpi::{PhysicalPosition, PhysicalSize},
    keyboard::{Key, KeyLocation, ModifiersKeyState, ModifiersState, PhysicalKey},
    window::{Theme, WindowId},
};
use std::path::PathBuf;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementState {
    Pressed,
    Released,
}
impl ElementState {
    pub fn is_pressed(self) -> bool {
        self == Self::Pressed
    }
}
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
    Other(u16),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchPhase {
    Started,
    Moved,
    Ended,
    Cancelled,
}
#[derive(Debug, Clone, Copy)]
pub enum MouseScrollDelta {
    LineDelta(f32, f32),
    PixelDelta(PhysicalPosition<f64>),
}
#[derive(Debug, Clone, Copy)]
pub enum Force {
    Normalized(f64),
    Calibrated {
        force: f64,
        max_possible_force: f64,
        altitude_angle: Option<f64>,
    },
}
#[derive(Debug, Clone, Copy)]
pub struct Touch {
    pub phase: TouchPhase,
    pub location: PhysicalPosition<f64>,
    pub force: Option<Force>,
    pub id: u64,
    pub device_id: DeviceId,
}
#[derive(Debug, Default, Clone, Copy)]
pub struct DeviceId;
impl DeviceId {
    pub const fn dummy() -> Self {
        Self
    }
}
#[derive(Debug, Default, Clone, Copy)]
pub struct Modifiers {
    pub state: ModifiersState,
    pub left_alt: bool,
    pub right_alt: bool,
}
impl Modifiers {
    pub fn state(self) -> ModifiersState {
        self.state
    }
    pub fn lalt_state(self) -> ModifiersKeyState {
        if self.left_alt {
            ModifiersKeyState::Pressed
        } else {
            ModifiersKeyState::Unknown
        }
    }
    pub fn ralt_state(self) -> ModifiersKeyState {
        if self.right_alt {
            ModifiersKeyState::Pressed
        } else {
            ModifiersKeyState::Unknown
        }
    }
}
impl From<ModifiersState> for Modifiers {
    fn from(state: ModifiersState) -> Self {
        Self {
            state,
            ..Self::default()
        }
    }
}
#[derive(Debug, Clone)]
pub struct KeyEvent {
    pub physical_key: PhysicalKey,
    pub logical_key: Key,
    pub text: Option<String>,
    pub unmodified_key: Key,
    pub location: KeyLocation,
    pub state: ElementState,
    pub repeat: bool,
}
impl KeyEvent {
    pub fn text_with_all_modifiers(&self) -> Option<&str> {
        self.text.as_deref()
    }
    pub fn key_without_modifiers(&self) -> Key {
        self.unmodified_key.clone()
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ime {
    Enabled,
    Preedit(String, Option<(usize, usize)>),
    Commit(String),
    Disabled,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartCause {
    Init,
    Poll,
    WaitCancelled,
    ResumeTimeReached,
}
#[derive(Debug, Clone)]
pub enum WindowEvent {
    Resized(PhysicalSize<u32>),
    Moved(PhysicalPosition<i32>),
    CloseRequested,
    Destroyed,
    DroppedFile(PathBuf),
    HoveredFile(PathBuf),
    HoveredFileCancelled,
    Focused(bool),
    Occluded(bool),
    ScaleFactorChanged {
        scale_factor: f64,
    },
    RedrawRequested,
    ThemeChanged(Theme),
    KeyboardInput {
        device_id: DeviceId,
        event: KeyEvent,
        is_synthetic: bool,
    },
    ModifiersChanged(Modifiers),
    Ime(Ime),
    CursorMoved {
        device_id: DeviceId,
        position: PhysicalPosition<f64>,
    },
    CursorEntered {
        device_id: DeviceId,
    },
    CursorLeft {
        device_id: DeviceId,
    },
    MouseWheel {
        device_id: DeviceId,
        delta: MouseScrollDelta,
        phase: TouchPhase,
    },
    MouseInput {
        device_id: DeviceId,
        state: ElementState,
        button: MouseButton,
    },
    Touch(Touch),
    ActivationTokenDone {},
    DoubleTapGesture {},
    TouchpadPressure {},
    RotationGesture {},
    PinchGesture {},
    AxisMotion {},
    PanGesture {},
}
#[derive(Debug, Clone)]
pub enum Event<T> {
    User(T),
    Window {
        window_id: WindowId,
        event: WindowEvent,
    },
    AboutToWait,
    Suspended,
    Resumed,
    NewEvents {
        cause: StartCause,
    },
    Device {},
    LoopExiting,
    MemoryWarning,
}
