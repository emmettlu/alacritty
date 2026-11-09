//! Native X11 windows and file-descriptor-driven event dispatch.
use super::{
    dpi::{PhysicalPosition, PhysicalSize},
    event::*,
    event_loop::{ControlFlow, emit},
    keyboard::*,
    monitor::MonitorHandle,
    window::*,
};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    os::fd::{AsFd, AsRawFd, OwnedFd},
    rc::Rc,
    sync::OnceLock,
    time::Instant,
};
use x11rb::{
    connection::Connection,
    protocol::{Event as XEvent, xproto::*},
    wrapper::ConnectionExt as _,
    xcb_ffi::XCBConnection,
};
struct Wake {
    read: OwnedFd,
    write: OwnedFd,
}
static WAKE: OnceLock<Wake> = OnceLock::new();
thread_local! { static DISPLAY:RefCell<Option<Rc<Display>>>=const { RefCell::new(None) }; }
pub struct Display {
    pub connection: XCBConnection,
    pub screen: usize,
    windows: RefCell<HashMap<u32, PhysicalSize<u32>>>,
    atoms: HashMap<&'static str, u32>,
    mapping: RefCell<Mapping>,
}
struct Mapping {
    first: u8,
    per_key: usize,
    symbols: Vec<u32>,
}
fn io(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(error.to_string())
}
impl Display {
    fn atom(&self, name: &str) -> u32 {
        self.atoms.get(name).copied().unwrap_or(0)
    }
    fn mapping(connection: &XCBConnection) -> std::io::Result<Mapping> {
        let setup = connection.setup();
        let first = setup.min_keycode;
        let reply = connection
            .get_keyboard_mapping(first, setup.max_keycode - first + 1)
            .map_err(io)?
            .reply()
            .map_err(io)?;
        Ok(Mapping {
            first,
            per_key: reply.keysyms_per_keycode as usize,
            symbols: reply.keysyms,
        })
    }
}
pub fn initialize() -> std::io::Result<()> {
    let (read, write) = rustix::pipe::pipe_with(
        rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
    )
    .map_err(io)?;
    let _ = WAKE.set(Wake { read, write });
    let (connection, screen) = XCBConnection::connect(None).map_err(io)?;
    let mut atoms = HashMap::new();
    for name in [
        "WM_PROTOCOLS",
        "WM_DELETE_WINDOW",
        "UTF8_STRING",
        "_NET_WM_NAME",
        "_NET_WM_STATE",
        "_NET_WM_STATE_FULLSCREEN",
        "_NET_WM_STATE_MAXIMIZED_VERT",
        "_NET_WM_STATE_MAXIMIZED_HORZ",
        "_NET_WM_STATE_ABOVE",
        "_NET_WM_STATE_HIDDEN",
        "_NET_WM_STATE_DEMANDS_ATTENTION",
        "_MOTIF_WM_HINTS",
        "_GTK_THEME_VARIANT",
        "_KDE_NET_WM_BLUR_BEHIND_REGION",
    ] {
        atoms.insert(
            name,
            connection
                .intern_atom(false, name.as_bytes())
                .map_err(io)?
                .reply()
                .map_err(io)?
                .atom,
        );
    }
    let mapping = RefCell::new(Display::mapping(&connection)?);
    DISPLAY.with_borrow_mut(|display| {
        *display = Some(Rc::new(Display {
            connection,
            screen,
            windows: RefCell::new(HashMap::new()),
            atoms,
            mapping,
        }))
    });
    Ok(())
}
pub fn wake() {
    if let Some(wake) = WAKE.get() {
        let _ = rustix::io::write(&wake.write, &[1]);
    }
}
pub fn wayland_display() -> Option<*mut std::ffi::c_void> {
    None
}
pub fn wait(flow: ControlFlow) {
    let timeout = match flow {
        ControlFlow::Poll => return,
        ControlFlow::Wait => None,
        ControlFlow::WaitUntil(time) => Some(
            rustix::event::Timespec::try_from(time.saturating_duration_since(Instant::now()))
                .unwrap_or(rustix::event::Timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                }),
        ),
    };
    DISPLAY.with_borrow(|display| {
        let Some(display) = display else { return };
        let Some(wake) = WAKE.get() else { return };
        let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(display.connection.as_raw_fd()) };
        let mut fds = [
            rustix::event::PollFd::new(&fd, rustix::event::PollFlags::IN),
            rustix::event::PollFd::new(wake.read.as_fd(), rustix::event::PollFlags::IN),
        ];
        let _ = rustix::event::poll(&mut fds, timeout.as_ref());
    });
}
fn mods(mask: KeyButMask) -> Modifiers {
    let mut state = ModifiersState::empty();
    if mask.contains(KeyButMask::SHIFT) {
        state |= ModifiersState::SHIFT;
    }
    if mask.contains(KeyButMask::CONTROL) {
        state |= ModifiersState::CONTROL;
    }
    if mask.contains(KeyButMask::MOD1) {
        state |= ModifiersState::ALT;
    }
    if mask.contains(KeyButMask::MOD4) {
        state |= ModifiersState::SUPER;
    }
    Modifiers {
        state,
        left_alt: state.alt_key(),
        right_alt: false,
    }
}
fn named(sym: u32) -> Option<NamedKey> {
    Some(match sym {
        0xff08 => NamedKey::Backspace,
        0xff09 | 0xfe20 => NamedKey::Tab,
        0xff0d | 0xff8d => NamedKey::Enter,
        0xff1b => NamedKey::Escape,
        0x20 => NamedKey::Space,
        0xff50 | 0xff95 => NamedKey::Home,
        0xff51 | 0xff96 => NamedKey::ArrowLeft,
        0xff52 | 0xff97 => NamedKey::ArrowUp,
        0xff53 | 0xff98 => NamedKey::ArrowRight,
        0xff54 | 0xff99 => NamedKey::ArrowDown,
        0xff55 | 0xff9a => NamedKey::PageUp,
        0xff56 | 0xff9b => NamedKey::PageDown,
        0xff57 | 0xff9c => NamedKey::End,
        0xff63 | 0xff9e => NamedKey::Insert,
        0xffff | 0xff9f => NamedKey::Delete,
        0xffe1 | 0xffe2 => NamedKey::Shift,
        0xffe3 | 0xffe4 => NamedKey::Control,
        0xffe9 | 0xffea => NamedKey::Alt,
        0xffeb | 0xffec => NamedKey::Super,
        0xffbe => NamedKey::F1,
        0xffbf => NamedKey::F2,
        0xffc0 => NamedKey::F3,
        0xffc1 => NamedKey::F4,
        0xffc2 => NamedKey::F5,
        0xffc3 => NamedKey::F6,
        0xffc4 => NamedKey::F7,
        0xffc5 => NamedKey::F8,
        0xffc6 => NamedKey::F9,
        0xffc7 => NamedKey::F10,
        0xffc8 => NamedKey::F11,
        0xffc9 => NamedKey::F12,
        _ => return None,
    })
}
fn character(sym: u32) -> Option<char> {
    let code = if let Some(xkb) = xkbcommon_dl::xkbcommon_option() {
        unsafe { (xkb.xkb_keysym_to_utf32)(sym) }
    } else if sym & 0xff000000 == 0x01000000 {
        sym & 0x00ffffff
    } else {
        sym
    };
    char::from_u32(code).filter(|c| !c.is_control())
}
fn keyboard(display: &Display, event: &KeyPressEvent, pressed: bool) -> KeyEvent {
    let mapping = display.mapping.borrow();
    let index = (event.detail.saturating_sub(mapping.first)) as usize * mapping.per_key;
    let base = mapping.symbols.get(index).copied().unwrap_or(0);
    let shift = event.state.contains(KeyButMask::SHIFT);
    let caps = event.state.contains(KeyButMask::LOCK);
    let upper = mapping
        .symbols
        .get(index + 1)
        .copied()
        .filter(|s| *s != 0)
        .unwrap_or(base);
    let symbol =
        if shift ^ caps && character(base).is_some_and(|c| c.is_alphabetic()) || shift && !caps {
            upper
        } else {
            base
        };
    let mut text = named(symbol)
        .and_then(NamedKey::to_text)
        .map(str::to_string)
        .or_else(|| character(symbol).map(|c| c.to_string()));
    let logical_key = named(symbol)
        .map(Key::Named)
        .unwrap_or_else(|| Key::Character(text.clone().unwrap_or_default()));
    if event.state.contains(KeyButMask::CONTROL) {
        if let Some(value) = text.as_mut() {
            if let Some(ch) = value.chars().next().filter(|c| c.is_ascii()) {
                let code = (ch as u8).to_ascii_uppercase();
                if (b'@'..=b'_').contains(&code) {
                    *value = String::from(code as char);
                    value.replace_range(.., std::str::from_utf8(&[code & 0x1f]).unwrap());
                }
            }
        }
    }
    let unmodified_key = named(base).map(Key::Named).unwrap_or_else(|| {
        Key::Character(
            character(base)
                .map(|c| c.to_lowercase().to_string())
                .unwrap_or_default(),
        )
    });
    let location = if (0xff80..=0xffbd).contains(&symbol) {
        KeyLocation::Numpad
    } else if matches!(symbol, 0xffe2 | 0xffe4 | 0xffea | 0xffec) {
        KeyLocation::Right
    } else if matches!(symbol, 0xffe1 | 0xffe3 | 0xffe9 | 0xffeb) {
        KeyLocation::Left
    } else {
        KeyLocation::Standard
    };
    KeyEvent {
        physical_key: PhysicalKey(event.detail.saturating_sub(8) as u32),
        logical_key,
        text,
        unmodified_key,
        location,
        state: if pressed {
            ElementState::Pressed
        } else {
            ElementState::Released
        },
        repeat: false,
    }
}
pub fn drain_events() {
    if let Some(wake) = WAKE.get() {
        let mut data = [0; 256];
        while rustix::io::read(&wake.read, &mut data).is_ok() {}
    }
    DISPLAY.with_borrow(|display| {
        let Some(display) = display else { return };
        loop {
            let event = match display.connection.poll_for_event() {
                Ok(Some(event)) => event,
                Ok(None) => break,
                Err(err) => {
                    log::error!("X11 event failure: {err}");
                    break;
                }
            };
            match event {
                XEvent::Expose(event) => {
                    emit(WindowId(event.window as u64), WindowEvent::RedrawRequested)
                }
                XEvent::ConfigureNotify(event) => {
                    let size = PhysicalSize::new(event.width as u32, event.height as u32);
                    let changed =
                        display.windows.borrow_mut().insert(event.window, size) != Some(size);
                    if changed {
                        emit(WindowId(event.window as u64), WindowEvent::Resized(size));
                    }
                }
                XEvent::ClientMessage(event) => {
                    if event.type_ == display.atom("WM_PROTOCOLS")
                        && event.data.as_data32()[0] == display.atom("WM_DELETE_WINDOW")
                    {
                        emit(WindowId(event.window as u64), WindowEvent::CloseRequested);
                    }
                }
                XEvent::FocusIn(event) => {
                    emit(WindowId(event.event as u64), WindowEvent::Focused(true))
                }
                XEvent::FocusOut(event) => {
                    emit(
                        WindowId(event.event as u64),
                        WindowEvent::ModifiersChanged(Modifiers::default()),
                    );
                    emit(WindowId(event.event as u64), WindowEvent::Focused(false));
                }
                XEvent::KeyPress(event) | XEvent::KeyRelease(event) => {
                    let pressed = event.response_type & 0x7f == KEY_PRESS_EVENT;
                    let id = WindowId(event.event as u64);
                    emit(id, WindowEvent::ModifiersChanged(mods(event.state)));
                    emit(
                        id,
                        WindowEvent::KeyboardInput {
                            device_id: DeviceId,
                            event: keyboard(display, &event, pressed),
                            is_synthetic: false,
                        },
                    );
                }
                XEvent::MotionNotify(event) => {
                    let id = WindowId(event.event as u64);
                    emit(id, WindowEvent::ModifiersChanged(mods(event.state)));
                    emit(
                        id,
                        WindowEvent::CursorMoved {
                            device_id: DeviceId,
                            position: PhysicalPosition::new(
                                event.event_x as f64,
                                event.event_y as f64,
                            ),
                        },
                    );
                }
                XEvent::LeaveNotify(event) => emit(
                    WindowId(event.event as u64),
                    WindowEvent::CursorLeft {
                        device_id: DeviceId,
                    },
                ),
                XEvent::ButtonPress(event) | XEvent::ButtonRelease(event) => {
                    let id = WindowId(event.event as u64);
                    let pressed = event.response_type & 0x7f == BUTTON_PRESS_EVENT;
                    emit(id, WindowEvent::ModifiersChanged(mods(event.state)));
                    if (4..=7).contains(&event.detail) {
                        if pressed {
                            let (x, y) = match event.detail {
                                4 => (0., 1.),
                                5 => (0., -1.),
                                6 => (1., 0.),
                                _ => (-1., 0.),
                            };
                            emit(
                                id,
                                WindowEvent::MouseWheel {
                                    device_id: DeviceId,
                                    delta: MouseScrollDelta::LineDelta(x, y),
                                    phase: TouchPhase::Moved,
                                },
                            );
                        }
                    } else {
                        let button = match event.detail {
                            1 => MouseButton::Left,
                            2 => MouseButton::Middle,
                            3 => MouseButton::Right,
                            8 => MouseButton::Back,
                            9 => MouseButton::Forward,
                            n => MouseButton::Other(n as u16),
                        };
                        emit(
                            id,
                            WindowEvent::MouseInput {
                                device_id: DeviceId,
                                state: if pressed {
                                    ElementState::Pressed
                                } else {
                                    ElementState::Released
                                },
                                button,
                            },
                        );
                    }
                }
                XEvent::MappingNotify(_) => {
                    if let Ok(mapping) = Display::mapping(&display.connection) {
                        *display.mapping.borrow_mut() = mapping;
                    }
                }
                _ => {}
            }
        }
    });
}
pub struct Window {
    pub display: Rc<Display>,
    pub xid: u32,
    scale: f64,
    maximized: Cell<bool>,
    fullscreen: Cell<bool>,
    cursor_visible: Cell<bool>,
}
impl Window {
    pub fn new(attrs: WindowAttributes) -> std::io::Result<Self> {
        DISPLAY.with_borrow(|display| {
            let display = display
                .as_ref()
                .ok_or_else(|| io("X11 display is not initialized"))?
                .clone();
            let conn = &display.connection;
            let screen = &conn.setup().roots[display.screen];
            let xid = conn.generate_id().map_err(io)?;
            let parent = attrs.embed.unwrap_or(screen.root);
            let mask = EventMask::EXPOSURE
                | EventMask::STRUCTURE_NOTIFY
                | EventMask::FOCUS_CHANGE
                | EventMask::KEY_PRESS
                | EventMask::KEY_RELEASE
                | EventMask::BUTTON_PRESS
                | EventMask::BUTTON_RELEASE
                | EventMask::POINTER_MOTION
                | EventMask::LEAVE_WINDOW
                | EventMask::PROPERTY_CHANGE;
            let visual = if attrs.transparent {
                screen
                    .allowed_depths
                    .iter()
                    .find(|d| d.depth == 32)
                    .and_then(|d| d.visuals.first())
                    .map(|v| v.visual_id)
            } else {
                None
            };
            let colormap = conn.generate_id().map_err(io)?;
            conn.create_colormap(
                ColormapAlloc::NONE,
                colormap,
                screen.root,
                visual.unwrap_or(screen.root_visual),
            )
            .map_err(io)?;
            let aux = CreateWindowAux::new()
                .event_mask(mask)
                .colormap(colormap)
                .border_pixel(0)
                .background_pixel(0);
            conn.create_window(
                if visual.is_some() {
                    32
                } else {
                    screen.root_depth
                },
                xid,
                parent,
                attrs.position.map_or(0, |p| p.x as i16),
                attrs.position.map_or(0, |p| p.y as i16),
                attrs.size.width as u16,
                attrs.size.height as u16,
                0,
                WindowClass::INPUT_OUTPUT,
                visual.unwrap_or(screen.root_visual),
                &aux,
            )
            .map_err(io)?
            .check()
            .map_err(io)?;
            conn.change_property32(
                PropMode::REPLACE,
                xid,
                display.atom("WM_PROTOCOLS"),
                AtomEnum::ATOM,
                &[display.atom("WM_DELETE_WINDOW")],
            )
            .map_err(io)?;
            let class = format!("{}\0{}\0", attrs.class.instance, attrs.class.general);
            conn.change_property8(
                PropMode::REPLACE,
                xid,
                AtomEnum::WM_CLASS,
                AtomEnum::STRING,
                class.as_bytes(),
            )
            .map_err(io)?;
            if attrs.decorations == crate::config::window::Decorations::None {
                conn.change_property32(
                    PropMode::REPLACE,
                    xid,
                    display.atom("_MOTIF_WM_HINTS"),
                    display.atom("_MOTIF_WM_HINTS"),
                    &[2, 0, 0, 0, 0],
                )
                .map_err(io)?;
            }
            display.windows.borrow_mut().insert(xid, attrs.size);
            let scale = if screen.width_in_millimeters > 0 {
                (screen.width_in_pixels as f64 * 25.4 / screen.width_in_millimeters as f64 / 96.)
                    .max(1.)
            } else {
                1.
            };
            let window = Self {
                display: display.clone(),
                xid,
                scale,
                maximized: Cell::new(false),
                fullscreen: Cell::new(false),
                cursor_visible: Cell::new(true),
            };
            window.set_title(&attrs.title);
            window.set_theme(attrs.theme);
            window.set_blur(attrs.blur);
            if attrs.maximized {
                window.set_maximized(true);
            }
            if attrs.fullscreen {
                window.set_fullscreen(Some(Fullscreen::Borderless(None)));
            }
            if attrs.level == WindowLevel::AlwaysOnTop {
                window.state(true, "_NET_WM_STATE_ABOVE", None);
            }
            conn.flush().map_err(io)?;
            Ok(window)
        })
    }
    fn state(&self, enabled: bool, first: &str, second: Option<&str>) {
        let event = ClientMessageEvent::new(
            32,
            self.xid,
            self.display.atom("_NET_WM_STATE"),
            [
                u32::from(enabled),
                self.display.atom(first),
                second.map_or(0, |s| self.display.atom(s)),
                1,
                0,
            ],
        );
        let root = self.display.connection.setup().roots[self.display.screen].root;
        let _ = self.display.connection.send_event(
            false,
            root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        );
        let _ = self.display.connection.flush();
    }
    pub fn xcb_connection(&self) -> *mut std::ffi::c_void {
        self.display.connection.get_raw_xcb_connection()
    }
    pub fn id(&self) -> WindowId {
        WindowId(self.xid as u64)
    }
    pub fn inner_size(&self) -> PhysicalSize<u32> {
        self.display
            .windows
            .borrow()
            .get(&self.xid)
            .copied()
            .unwrap_or_default()
    }
    pub fn scale_factor(&self) -> f64 {
        self.scale
    }
    pub fn request_inner_size(&self, size: PhysicalSize<u32>) -> Option<PhysicalSize<u32>> {
        let _ = self.display.connection.configure_window(
            self.xid,
            &ConfigureWindowAux::new()
                .width(size.width)
                .height(size.height),
        );
        let _ = self.display.connection.flush();
        self.display.windows.borrow_mut().insert(self.xid, size);
        Some(size)
    }
    pub fn set_visible(&self, visible: bool) {
        if visible {
            let _ = self.display.connection.map_window(self.xid);
        } else {
            let _ = self.display.connection.unmap_window(self.xid);
        }
        let _ = self.display.connection.flush();
    }
    pub fn set_title(&self, title: &str) {
        let conn = &self.display.connection;
        let _ = conn.change_property8(
            PropMode::REPLACE,
            self.xid,
            self.display.atom("_NET_WM_NAME"),
            self.display.atom("UTF8_STRING"),
            title.as_bytes(),
        );
        let _ = conn.change_property8(
            PropMode::REPLACE,
            self.xid,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            title.as_bytes(),
        );
        let _ = conn.flush();
    }
    pub fn request_redraw(&self) {
        emit(self.id(), WindowEvent::RedrawRequested);
    }
    pub fn set_cursor(&self, cursor: CursorIcon) {
        let conn = &self.display.connection;
        if let (Ok(font), Ok(id)) = (conn.generate_id(), conn.generate_id()) {
            let _ = conn.open_font(font, b"cursor");
            let glyph = match cursor {
                CursorIcon::Default => 68,
                CursorIcon::Text => 152,
                CursorIcon::Pointer => 60,
            };
            let _ = conn.create_glyph_cursor(
                id,
                font,
                font,
                glyph,
                glyph + 1,
                0,
                0,
                0,
                u16::MAX,
                u16::MAX,
                u16::MAX,
            );
            let _ = conn
                .change_window_attributes(self.xid, &ChangeWindowAttributesAux::new().cursor(id));
            let _ = conn.close_font(font);
            let _ = conn.free_cursor(id);
            let _ = conn.flush();
        }
    }
    pub fn set_cursor_visible(&self, visible: bool) {
        if self.cursor_visible.replace(visible) == visible {
            return;
        }
        use x11rb::protocol::xfixes::ConnectionExt as _;
        if visible {
            let _ = self.display.connection.xfixes_show_cursor(self.xid);
        } else {
            let _ = self.display.connection.xfixes_hide_cursor(self.xid);
        }
        let _ = self.display.connection.flush();
    }
    pub fn set_ime_allowed(&self, _allowed: bool) {}
    pub fn set_ime_purpose(&self, _purpose: ImePurpose) {}
    pub fn set_ime_cursor_area(&self, _position: PhysicalPosition<f64>, _size: PhysicalSize<f64>) {}
    pub fn set_transparent(&self, _transparent: bool) {}
    pub fn set_blur(&self, blur: bool) {
        if blur {
            let _ = self.display.connection.change_property32(
                PropMode::REPLACE,
                self.xid,
                self.display.atom("_KDE_NET_WM_BLUR_BEHIND_REGION"),
                AtomEnum::CARDINAL,
                &[],
            );
        } else {
            let _ = self.display.connection.delete_property(
                self.xid,
                self.display.atom("_KDE_NET_WM_BLUR_BEHIND_REGION"),
            );
        }
        let _ = self.display.connection.flush();
    }
    pub fn set_theme(&self, theme: Option<Theme>) {
        let value = if theme == Some(Theme::Dark) {
            b"dark".as_slice()
        } else {
            b"light".as_slice()
        };
        let _ = self.display.connection.change_property8(
            PropMode::REPLACE,
            self.xid,
            self.display.atom("_GTK_THEME_VARIANT"),
            self.display.atom("UTF8_STRING"),
            value,
        );
    }
    pub fn set_maximized(&self, maximized: bool) {
        self.maximized.set(maximized);
        self.state(
            maximized,
            "_NET_WM_STATE_MAXIMIZED_VERT",
            Some("_NET_WM_STATE_MAXIMIZED_HORZ"),
        );
    }
    pub fn is_maximized(&self) -> bool {
        self.maximized.get()
    }
    pub fn set_minimized(&self, minimized: bool) {
        if minimized {
            let event = ClientMessageEvent::new(
                32,
                self.xid,
                self.display
                    .connection
                    .intern_atom(false, b"WM_CHANGE_STATE")
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .map_or(0, |r| r.atom),
                [3, 0, 0, 0, 0],
            );
            let root = self.display.connection.setup().roots[self.display.screen].root;
            let _ = self.display.connection.send_event(
                false,
                root,
                EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
                event,
            );
        } else {
            let _ = self.display.connection.map_window(self.xid);
        }
        let _ = self.display.connection.flush();
    }
    pub fn set_resize_increments(&self, increments: Option<PhysicalSize<f32>>) {
        let mut hints = [0u32; 18];
        if let Some(increments) = increments {
            hints[0] = 1 << 6;
            hints[9] = increments.width.max(1.) as u32;
            hints[10] = increments.height.max(1.) as u32;
        }
        let _ = self.display.connection.change_property32(
            PropMode::REPLACE,
            self.xid,
            AtomEnum::WM_NORMAL_HINTS,
            AtomEnum::WM_SIZE_HINTS,
            &hints,
        );
    }
    pub fn fullscreen(&self) -> Option<Fullscreen> {
        self.fullscreen
            .get()
            .then_some(Fullscreen::Borderless(None))
    }
    pub fn set_fullscreen(&self, mode: Option<Fullscreen>) {
        self.fullscreen.set(mode.is_some());
        self.state(mode.is_some(), "_NET_WM_STATE_FULLSCREEN", None);
    }
    pub fn current_monitor(&self) -> Option<MonitorHandle> {
        None
    }
    pub fn pre_present_notify(&self) {}
    pub fn request_user_attention(&self, attention: Option<UserAttentionType>) {
        self.state(attention.is_some(), "_NET_WM_STATE_DEMANDS_ATTENTION", None);
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        self.display.windows.borrow_mut().remove(&self.xid);
        let _ = self.display.connection.destroy_window(self.xid);
        let _ = self.display.connection.flush();
    }
}
