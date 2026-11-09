//! AppKit windows and input, using cidre exclusively.
use cidre::ns::WindowDelegate as _;
use cidre::objc::Obj;
use cidre::{arc, ca, cg, define_obj_type, ns, objc};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::Instant;

fn pool<T>(f: impl FnOnce() -> T) -> T {
    let mut value = None;
    objc::ar_pool(|| {
        value = Some(f());
    });
    value.unwrap()
}
use super::{
    dpi::{PhysicalPosition, PhysicalSize},
    event::*,
    event_loop::{ControlFlow, emit},
    keyboard::*,
    monitor::MonitorHandle,
    window::*,
};

trait AppApi: objc::Obj {
    #[objc::msg_send(finishLaunching)]
    fn finish_launching(&mut self);
    #[objc::msg_send(nextEventMatchingMask:untilDate:inMode:dequeue:)]
    fn next_event(
        &mut self,
        mask: usize,
        until: &ns::Date,
        mode: &ns::String,
        dequeue: bool,
    ) -> Option<arc::R<ns::Event>>;
    #[objc::msg_send(sendEvent:)]
    fn send_event(&mut self, event: &ns::Event);
    #[objc::msg_send(postEvent:atStart:)]
    fn post_event(&self, event: &ns::Event, at_start: bool);
    #[objc::msg_send(hide:)]
    fn hide(&self, sender: Option<&ns::Id>);
    #[objc::msg_send(hideOtherApplications:)]
    fn hide_others(&self, sender: Option<&ns::Id>);
}
impl AppApi for ns::App {}
trait AttentionApi: objc::Obj {
    #[objc::msg_send(requestUserAttention:)]
    fn attention(&self, kind: usize) -> isize;
    #[objc::msg_send(cancelUserAttentionRequest:)]
    fn cancel_attention(&self, id: isize);
}
impl AttentionApi for ns::App {}
define_obj_type!(Appearance(ns::Id));
impl Appearance {
    fn cls_ptr() -> *const std::ffi::c_void {
        unsafe { objc::objc_getClass(c"NSAppearance".as_ptr().cast()).unwrap() as *const _ as _ }
    }
    #[objc::msg_send(appearanceNamed:)]
    fn named(name: &ns::String) -> Option<arc::R<Self>>;
}
define_obj_type!(TerminalWindow(ns::Window), (), TERMINAL_WINDOW);
#[objc::add_methods]
impl TerminalWindow {
    #[objc::overrides(canBecomeKeyWindow)]
    fn can_become_key_window(&self) -> bool {
        true
    }
    #[objc::overrides(canBecomeMainWindow)]
    fn can_become_main_window(&self) -> bool {
        true
    }
}
define_obj_type!(
    ApplicationDelegate(ns::Id) + ns::AppDelegateImpl,
    (),
    TERMINAL_APP_DELEGATE
);
impl ns::AppDelegate for ApplicationDelegate {}
#[objc::add_methods]
impl ns::AppDelegateImpl for ApplicationDelegate {}
#[objc::add_methods]
impl ApplicationDelegate {
    #[objc::overrides(applicationShouldTerminate:)]
    fn should_terminate(&self, _app: &ns::App) -> usize {
        WINDOWS.with_borrow(|windows| {
            for window in windows.iter().filter_map(Weak::upgrade) {
                emit(window.id, WindowEvent::CloseRequested);
            }
        });
        0
    }
    #[objc::overrides(applicationShouldHandleReopen:hasVisibleWindows:)]
    fn reopen(&self, _app: &ns::App, _visible: bool) -> bool {
        WINDOWS.with_borrow(|windows| {
            for window in windows.iter().filter_map(Weak::upgrade) {
                window.window.borrow_mut().make_key_and_order_front(None);
            }
        });
        true
    }
}
thread_local! { static APPLICATION_DELEGATE: RefCell<Option<arc::R<ApplicationDelegate>>> = const { RefCell::new(None) }; }
fn menu(app: &mut ns::App) {
    let mut menu = ns::Menu::with_title(ns::str!(c"Alacritty"));
    let mut application = ns::Menu::with_title(ns::str!(c"Alacritty"));
    let selector = |name: &std::ffi::CStr| Some(unsafe { objc::sel_reg_name(name.as_ptr()) });
    application.add_item_with_title_action_key_equivalent(
        ns::str!(c"Hide Alacritty"),
        selector(c"hide:"),
        ns::str!(c"h"),
    );
    application.add_item(&ns::MenuItem::separator_item());
    application.add_item_with_title_action_key_equivalent(
        ns::str!(c"Quit Alacritty"),
        selector(c"terminate:"),
        ns::str!(c"q"),
    );
    let mut item =
        ns::MenuItem::with_title_action_key_equivalent(ns::str!(c"Alacritty"), None, ns::str!(c""));
    item.set_submenu(Some(&application));
    menu.add_item(&item);
    app.set_main_menu(Some(&menu));
}
trait WindowApi: objc::Obj {
    #[objc::msg_send(setContentSize:)]
    fn set_content_size(&self, size: cg::Size);
    #[objc::msg_send(orderOut:)]
    fn order_out(&self, sender: Option<&ns::Id>);
    #[objc::msg_send(miniaturize:)]
    fn miniaturize(&self, sender: Option<&ns::Id>);
    #[objc::msg_send(deminiaturize:)]
    fn deminiaturize(&self, sender: Option<&ns::Id>);
    #[objc::msg_send(zoom:)]
    fn zoom(&self, sender: Option<&ns::Id>);
    #[objc::msg_send(isZoomed)]
    fn is_zoomed(&self) -> bool;
    #[objc::msg_send(setContentResizeIncrements:)]
    fn set_increments(&self, size: cg::Size);
    #[objc::msg_send(setAcceptsMouseMovedEvents:)]
    fn set_accepts_mouse_moved(&self, value: bool);
    #[objc::msg_send(setTabbingIdentifier:)]
    fn set_tabbing_id(&self, value: &ns::String);
    #[objc::msg_send(tabbingIdentifier)]
    fn tabbing_id(&self) -> arc::R<ns::String>;
    #[objc::msg_send(selectNextTab:)]
    fn next_tab(&self, sender: Option<&ns::Id>);
    #[objc::msg_send(selectPreviousTab:)]
    fn previous_tab(&self, sender: Option<&ns::Id>);
    #[objc::msg_send(tabbedWindows)]
    fn tabbed_windows(&self) -> Option<arc::R<ns::Array<ns::Window>>>;
    #[objc::msg_send(setBackgroundColor:)]
    fn set_background_color(&self, color: &ns::Color);
    #[objc::msg_send(setAppearance:)]
    fn set_appearance(&self, appearance: Option<&Appearance>);
    #[objc::msg_send(standardWindowButton:)]
    fn standard_button(&self, kind: usize) -> Option<arc::R<ns::Button>>;
    #[objc::msg_send(screen)]
    fn screen(&self) -> Option<arc::R<ns::Screen>>;
}
impl WindowApi for ns::Window {}
trait ScreenApi: objc::Obj {
    #[objc::msg_send(maximumFramesPerSecond)]
    fn fps(&self) -> isize;
}
impl ScreenApi for ns::Screen {}
trait ViewApi: objc::Obj {
    #[objc::msg_send(interpretKeyEvents:)]
    fn interpret_keys(&mut self, events: &ns::Array<ns::Event>);
}
impl ViewApi for TerminalView {}
define_obj_type!(Cursor(ns::Id));
impl Cursor {
    fn cls_ptr() -> *const std::ffi::c_void {
        unsafe { objc::objc_getClass(c"NSCursor".as_ptr().cast()).unwrap() as *const _ as _ }
    }
    #[objc::msg_send(set)]
    fn set_cursor(&self);
    #[objc::msg_send(hide)]
    fn hide_cursor();
    #[objc::msg_send(unhide)]
    fn unhide_cursor();
    #[objc::msg_send(arrowCursor)]
    fn arrow() -> arc::R<Self>;
    #[objc::msg_send(IBeamCursor)]
    fn text() -> arc::R<Self>;
    #[objc::msg_send(pointingHandCursor)]
    fn pointer() -> arc::R<Self>;
}
trait LayerApi: objc::Obj {
    #[objc::msg_send(setOpaque:)]
    fn set_opaque(&mut self, value: bool);
}
impl LayerApi for ca::MetalLayer {}

define_obj_type!(EventFactory(ns::Event));
impl EventFactory {
    fn cls_ptr() -> *const std::ffi::c_void {
        ns::Event::cls_ptr()
    }
    #[objc::msg_send(otherEventWithType:location:modifierFlags:timestamp:windowNumber:context:subtype:data1:data2:)]
    fn wake_event(
        kind: usize,
        point: cg::Point,
        flags: usize,
        time: f64,
        window: isize,
        context: Option<&ns::Id>,
        subtype: i16,
        data1: isize,
        data2: isize,
    ) -> Option<arc::R<ns::Event>>;
}
static APP: AtomicPtr<ns::App> = AtomicPtr::new(std::ptr::null_mut());
thread_local! { static WINDOWS: RefCell<Vec<Weak<WindowState>>> = const { RefCell::new(Vec::new()) }; }

pub fn initialize() -> std::io::Result<()> {
    objc::ar_pool(|| {
        let mut app = ns::App::shared();
        app.set_activation_policy(ns::AppActivationPolicy::Regular);
        let delegate = ApplicationDelegate::new();
        app.set_delegate(Some(delegate.as_ref()));
        APPLICATION_DELEGATE.with_borrow_mut(|value| *value = Some(delegate));
        menu(&mut app);
        app.finish_launching();
        APP.store(app.as_ptr(), Ordering::Release);
        app.activate_ignoring_other_apps(true);
    });
    Ok(())
}
pub fn wake() {
    let app = APP.load(Ordering::Acquire);
    if app.is_null() {
        return;
    }
    objc::ar_pool(|| {
        if let Some(event) =
            EventFactory::wake_event(15, cg::Point::new(0., 0.), 0, 0., 0, None, 0, 0, 0)
        {
            // AppKit explicitly permits postEvent from a background thread.
            unsafe { &*app }.post_event(&event, false);
        }
    });
}
pub fn hide(others: bool) {
    let app = ns::App::shared();
    if others {
        app.hide_others(None)
    } else {
        app.hide(None)
    }
}
fn dispatch(event: &ns::Event) {
    let mut app = ns::App::shared();
    app.send_event(event);
}
pub fn drain_events() {
    objc::ar_pool(|| {
        let mut app = ns::App::shared();
        while let Some(event) = app.next_event(
            usize::MAX,
            &ns::Date::distant_past(),
            ns::str!(c"kCFRunLoopDefaultMode"),
            true,
        ) {
            dispatch(&event);
        }
        app.update_windows();
        WINDOWS.with_borrow_mut(|windows| {
            windows.retain(|window| {
                let Some(window) = window.upgrade() else {
                    return false;
                };
                window.update();
                true
            });
        });
    });
}
pub fn wait(flow: ControlFlow) {
    if matches!(flow, ControlFlow::Poll) {
        return;
    }
    objc::ar_pool(|| {
        let deadline = match flow {
            ControlFlow::WaitUntil(time) => ns::Date::with_time_interval_since_now(
                time.saturating_duration_since(Instant::now()).as_secs_f64(),
            ),
            _ => ns::Date::distant_future(),
        };
        if let Some(event) = ns::App::shared().next_event(
            usize::MAX,
            &deadline,
            ns::str!(c"kCFRunLoopDefaultMode"),
            true,
        ) {
            dispatch(&event);
        }
    });
}

struct ViewState {
    id: WindowId,
    marked: String,
    selection: ns::Range,
    ime_allowed: bool,
    ime_rect: cg::Rect,
    key: Option<KeyEvent>,
    option_as_alt: OptionAsAlt,
}
define_obj_type!(TerminalView(ns::View), ViewState, TERMINAL_VIEW);
fn text_value(value: &ns::Id) -> String {
    if let Some(value) = value.try_cast(ns::String::cls()) {
        value.to_string()
    } else if let Some(value) = value.try_cast(ns::AttrString::cls()) {
        value.string().to_string()
    } else {
        String::new()
    }
}
#[objc::add_methods]
impl TerminalView {
    #[objc::overrides(acceptsFirstResponder)]
    fn accepts_first_responder(&self) -> bool {
        true
    }
    #[objc::overrides(isFlipped)]
    fn is_flipped(&self) -> bool {
        true
    }
    #[objc::overrides(keyDown:)]
    fn key_down(&mut self, event: &ns::Event) {
        let key = key_event(event, ElementState::Pressed);
        let modifiers = modifiers(event.modifier_flags().0);
        emit(self.inner().id, WindowEvent::ModifiersChanged(modifiers));
        let alt = self.inner().option_as_alt;
        let flags = event.modifier_flags().0;
        let option_as_alt = alt == OptionAsAlt::Both
            || (alt == OptionAsAlt::OnlyLeft && flags & 0x20 != 0)
            || (alt == OptionAsAlt::OnlyRight && flags & 0x40 != 0);
        if !self.inner().ime_allowed
            || modifiers.state.control_key()
            || modifiers.state.super_key()
            || (modifiers.state.alt_key() && option_as_alt)
        {
            let mut key = key;
            if modifiers.state.alt_key() && option_as_alt {
                let text = event.characters_ignoring_modifiers().map(|s| s.to_string());
                key.logical_key = key.unmodified_key.clone();
                if let Some(text) = text {
                    key.text = Some(text);
                }
            }
            emit_key(self.inner().id, key);
            return;
        }
        self.inner_mut().key = Some(key);
        let events = ns::Array::from_slice(&[event]);
        self.interpret_keys(&events);
        if let Some(key) = self.inner_mut().key.take() {
            emit_key(self.inner().id, key);
        }
    }
    #[objc::overrides(keyUp:)]
    fn key_up(&mut self, event: &ns::Event) {
        emit_key(self.inner().id, key_event(event, ElementState::Released));
    }
    #[objc::overrides(flagsChanged:)]
    fn flags_changed(&mut self, event: &ns::Event) {
        let flags = event.modifier_flags().0;
        emit(
            self.inner().id,
            WindowEvent::ModifiersChanged(modifiers(flags)),
        );
        let mask = match event.key_code() {
            56 => 2,
            60 => 4,
            59 => 1,
            62 => 0x2000,
            58 => 0x20,
            61 => 0x40,
            55 => 8,
            54 => 0x10,
            57 => 1 << 16,
            _ => 0,
        };
        let state = if flags & mask != 0 {
            ElementState::Pressed
        } else {
            ElementState::Released
        };
        emit_key(self.inner().id, key_event(event, state));
    }
    #[objc::overrides(insertText:replacementRange:)]
    fn insert_text(&mut self, value: &ns::Id, _replacement: ns::Range) {
        let text = text_value(value);
        let state = self.inner_mut();
        if state.marked.is_empty() {
            if let Some(mut key) = state.key.take() {
                key.text = Some(text);
                emit_key(state.id, key);
                return;
            }
        }
        state.key = None;
        state.marked.clear();
        emit(
            state.id,
            WindowEvent::Ime(Ime::Preedit(String::new(), None)),
        );
        emit(state.id, WindowEvent::Ime(Ime::Commit(text)));
    }
    #[objc::overrides(setMarkedText:selectedRange:replacementRange:)]
    fn set_marked_text(&mut self, value: &ns::Id, selection: ns::Range, _replacement: ns::Range) {
        let text = text_value(value);
        let state = self.inner_mut();
        state.key = None;
        let start = utf16_to_byte(&text, selection.loc);
        let end = utf16_to_byte(&text, selection.loc.saturating_add(selection.len));
        state.marked = text.clone();
        state.selection = selection;
        emit(
            state.id,
            WindowEvent::Ime(Ime::Preedit(text, Some((start, end)))),
        );
    }
    #[objc::overrides(unmarkText)]
    fn unmark_text(&mut self) {
        let state = self.inner_mut();
        state.marked.clear();
        emit(
            state.id,
            WindowEvent::Ime(Ime::Preedit(String::new(), None)),
        );
    }
    #[objc::overrides(hasMarkedText)]
    fn has_marked_text(&self) -> bool {
        !self.inner().marked.is_empty()
    }
    #[objc::overrides(markedRange)]
    fn marked_range(&self) -> ns::Range {
        if self.inner().marked.is_empty() {
            ns::Range::new(usize::MAX, 0)
        } else {
            ns::Range::new(0, self.inner().marked.encode_utf16().count())
        }
    }
    #[objc::overrides(selectedRange)]
    fn selected_range(&self) -> ns::Range {
        self.inner().selection
    }
    #[objc::overrides(validAttributesForMarkedText)]
    fn valid_attributes(&self) -> arc::R<ns::Array<ns::String>> {
        ns::Array::new()
    }
    #[objc::overrides(attributedSubstringForProposedRange:actualRange:)]
    fn attributed_substring(
        &self,
        _range: ns::Range,
        _actual: *mut ns::Range,
    ) -> Option<arc::R<ns::AttrString>> {
        None
    }
    #[objc::overrides(characterIndexForPoint:)]
    fn character_index(&self, _point: cg::Point) -> usize {
        0
    }
    #[objc::overrides(firstRectForCharacterRange:actualRange:)]
    fn first_rect(&self, _range: ns::Range, _actual: *mut ns::Range) -> cg::Rect {
        let rect = self.convert_rect_to_view(self.inner().ime_rect, None);
        if let Some(window) = self.window() {
            window.rect_to_screen(rect)
        } else {
            rect
        }
    }
    #[objc::overrides(doCommandBySelector:)]
    fn command(&mut self, _selector: &objc::Sel) {
        if let Some(key) = self.inner_mut().key.take() {
            emit_key(self.inner().id, key);
        }
    }
    #[objc::overrides(mouseDown:)]
    fn mouse_down(&self, event: &ns::Event) {
        self.mouse(event, ElementState::Pressed);
    }
    #[objc::overrides(mouseUp:)]
    fn mouse_up(&self, event: &ns::Event) {
        self.mouse(event, ElementState::Released);
    }
    #[objc::overrides(rightMouseDown:)]
    fn right_down(&self, event: &ns::Event) {
        self.mouse(event, ElementState::Pressed);
    }
    #[objc::overrides(rightMouseUp:)]
    fn right_up(&self, event: &ns::Event) {
        self.mouse(event, ElementState::Released);
    }
    #[objc::overrides(otherMouseDown:)]
    fn other_down(&self, event: &ns::Event) {
        self.mouse(event, ElementState::Pressed);
    }
    #[objc::overrides(otherMouseUp:)]
    fn other_up(&self, event: &ns::Event) {
        self.mouse(event, ElementState::Released);
    }
    #[objc::overrides(mouseMoved:)]
    fn mouse_moved(&self, event: &ns::Event) {
        self.motion(event);
    }
    #[objc::overrides(mouseDragged:)]
    fn mouse_dragged(&self, event: &ns::Event) {
        self.motion(event);
    }
    #[objc::overrides(rightMouseDragged:)]
    fn right_dragged(&self, event: &ns::Event) {
        self.motion(event);
    }
    #[objc::overrides(otherMouseDragged:)]
    fn other_dragged(&self, event: &ns::Event) {
        self.motion(event);
    }
    #[objc::overrides(scrollWheel:)]
    fn scroll(&self, event: &ns::Event) {
        let scale = self.window().map_or(1., |w| w.backing_scale_factor());
        let delta = if event.has_precise_scrolling_deltas() {
            MouseScrollDelta::PixelDelta(PhysicalPosition::new(
                event.scrolling_delta_x() * scale,
                event.scrolling_delta_y() * scale,
            ))
        } else {
            MouseScrollDelta::LineDelta(
                event.scrolling_delta_x() as f32,
                event.scrolling_delta_y() as f32,
            )
        };
        let phase = event.phase().0;
        let phase = if phase & 1 != 0 {
            TouchPhase::Started
        } else if phase & 8 != 0 {
            TouchPhase::Ended
        } else if phase & 16 != 0 {
            TouchPhase::Cancelled
        } else {
            TouchPhase::Moved
        };
        emit(
            self.inner().id,
            WindowEvent::MouseWheel {
                device_id: DeviceId,
                delta,
                phase,
            },
        );
    }
    fn motion(&self, event: &ns::Event) {
        let point = self.convert_point_from_view(event.location_in_window(), None);
        let scale = self.window().map_or(1., |w| w.backing_scale_factor());
        emit(
            self.inner().id,
            WindowEvent::CursorMoved {
                device_id: DeviceId,
                position: PhysicalPosition::new(point.x * scale, point.y * scale),
            },
        );
    }
    fn mouse(&self, event: &ns::Event, state: ElementState) {
        self.motion(event);
        let button = match event.button_number() {
            0 => MouseButton::Left,
            1 => MouseButton::Right,
            2 => MouseButton::Middle,
            3 => MouseButton::Back,
            4 => MouseButton::Forward,
            n => MouseButton::Other(n as u16),
        };
        emit(
            self.inner().id,
            WindowEvent::MouseInput {
                device_id: DeviceId,
                state,
                button,
            },
        );
    }
}
trait WindowRectApi: objc::Obj {
    #[objc::msg_send(convertRectToScreen:)]
    fn rect_to_screen(&self, rect: cg::Rect) -> cg::Rect;
}
impl WindowRectApi for ns::Window {}
fn utf16_to_byte(text: &str, offset: usize) -> usize {
    let mut units = 0;
    for (byte, ch) in text.char_indices() {
        if units >= offset {
            return byte;
        }
        units += ch.len_utf16();
    }
    text.len()
}
fn emit_key(id: WindowId, event: KeyEvent) {
    emit(
        id,
        WindowEvent::KeyboardInput {
            device_id: DeviceId,
            event,
            is_synthetic: false,
        },
    );
}
fn modifiers(flags: usize) -> Modifiers {
    let mut state = ModifiersState::empty();
    if flags & (1 << 17) != 0 {
        state |= ModifiersState::SHIFT;
    }
    if flags & (1 << 18) != 0 {
        state |= ModifiersState::CONTROL;
    }
    if flags & (1 << 19) != 0 {
        state |= ModifiersState::ALT;
    }
    if flags & (1 << 20) != 0 {
        state |= ModifiersState::SUPER;
    }
    Modifiers {
        state,
        left_alt: flags & 0x20 != 0,
        right_alt: flags & 0x40 != 0,
    }
}
fn key_event(event: &ns::Event, state: ElementState) -> KeyEvent {
    let code = event.key_code();
    let named = match code {
        36 | 76 => Some(NamedKey::Enter),
        48 => Some(NamedKey::Tab),
        49 => Some(NamedKey::Space),
        51 => Some(NamedKey::Backspace),
        53 => Some(NamedKey::Escape),
        55 | 54 => Some(NamedKey::Super),
        56 | 60 => Some(NamedKey::Shift),
        58 | 61 => Some(NamedKey::Alt),
        59 | 62 => Some(NamedKey::Control),
        57 => Some(NamedKey::CapsLock),
        123 => Some(NamedKey::ArrowLeft),
        124 => Some(NamedKey::ArrowRight),
        125 => Some(NamedKey::ArrowDown),
        126 => Some(NamedKey::ArrowUp),
        115 => Some(NamedKey::Home),
        119 => Some(NamedKey::End),
        116 => Some(NamedKey::PageUp),
        121 => Some(NamedKey::PageDown),
        117 => Some(NamedKey::Delete),
        114 => Some(NamedKey::Insert),
        122 => Some(NamedKey::F1),
        120 => Some(NamedKey::F2),
        99 => Some(NamedKey::F3),
        118 => Some(NamedKey::F4),
        96 => Some(NamedKey::F5),
        97 => Some(NamedKey::F6),
        98 => Some(NamedKey::F7),
        100 => Some(NamedKey::F8),
        101 => Some(NamedKey::F9),
        109 => Some(NamedKey::F10),
        103 => Some(NamedKey::F11),
        111 => Some(NamedKey::F12),
        105 => Some(NamedKey::F13),
        107 => Some(NamedKey::F14),
        113 => Some(NamedKey::F15),
        106 => Some(NamedKey::F16),
        64 => Some(NamedKey::F17),
        79 => Some(NamedKey::F18),
        80 => Some(NamedKey::F19),
        90 => Some(NamedKey::F20),
        _ => None,
    };
    let modifier_event = event.type_() == ns::EventType::FlagsChanged;
    let unmodified = (!modifier_event)
        .then(|| event.characters_ignoring_modifiers())
        .flatten()
        .map(|s| s.to_string())
        .unwrap_or_default();
    let text = (!modifier_event)
        .then(|| event.characters())
        .flatten()
        .map(|s| s.to_string())
        .filter(|s| !s.chars().any(|c| ('\u{f700}'..='\u{f8ff}').contains(&c)));
    let logical_key = named
        .map(Key::Named)
        .unwrap_or_else(|| Key::Character(text.clone().unwrap_or_else(|| unmodified.clone())));
    let unmodified_key = named
        .map(Key::Named)
        .unwrap_or_else(|| Key::Character(unmodified.to_lowercase()));
    let location = match code {
        54 | 60 | 61 | 62 => KeyLocation::Right,
        55 | 56 | 58 | 59 => KeyLocation::Left,
        65 | 67 | 69 | 71 | 75 | 76 | 78 | 81..=92 => KeyLocation::Numpad,
        _ => KeyLocation::Standard,
    };
    KeyEvent {
        physical_key: PhysicalKey(code as u32),
        logical_key,
        text,
        unmodified_key,
        location,
        state,
        repeat: !modifier_event && state == ElementState::Pressed && event.is_a_repeat(),
    }
}

define_obj_type!(
    WindowDelegate(ns::Id) + ns::WindowDelegateImpl,
    WindowId,
    TERMINAL_WINDOW_DELEGATE
);
impl ns::WindowDelegate for WindowDelegate {}
#[objc::add_methods]
impl ns::WindowDelegateImpl for WindowDelegate {
    extern "C" fn impl_window_should_close(
        &mut self,
        _cmd: Option<&objc::Sel>,
        _window: &ns::Window,
    ) -> bool {
        emit(*self.inner(), WindowEvent::CloseRequested);
        false
    }
}
struct WindowState {
    window: RefCell<arc::R<TerminalWindow>>,
    view: RefCell<arc::R<TerminalView>>,
    layer: RefCell<arc::R<ca::MetalLayer>>,
    blur: RefCell<arc::R<ns::VisualEffectView>>,
    attention: Cell<isize>,
    _delegate: arc::R<WindowDelegate>,
    id: WindowId,
    size: Cell<PhysicalSize<u32>>,
    scale: Cell<f64>,
    focused: Cell<bool>,
    occluded: Cell<bool>,
    cursor_visible: Cell<bool>,
    simple_fullscreen: Cell<bool>,
    saved_frame: Cell<Option<cg::Rect>>,
}
impl WindowState {
    fn update(&self) {
        let window = self.window.borrow();
        let scale = window.backing_scale_factor();
        let bounds = self.view.borrow().bounds();
        let size = PhysicalSize::new(
            (bounds.size.width * scale).round() as u32,
            (bounds.size.height * scale).round() as u32,
        );
        if self.scale.replace(scale) != scale {
            emit(
                self.id,
                WindowEvent::ScaleFactorChanged {
                    scale_factor: scale,
                },
            );
        }
        if self.size.replace(size) != size {
            emit(self.id, WindowEvent::Resized(size));
        }
        let focus = window.is_key_window();
        if self.focused.replace(focus) != focus {
            emit(self.id, WindowEvent::Focused(focus));
        }
        let occluded = window.occlusion_state().0 & 2 == 0;
        if self.occluded.replace(occluded) != occluded {
            emit(self.id, WindowEvent::Occluded(occluded));
        }
        let mut layer = self.layer.borrow_mut();
        layer.set_contents_scale(scale);
        layer.set_drawable_size(cg::Size::new(size.width as f64, size.height as f64));
    }
}
pub struct Window {
    state: Rc<WindowState>,
}
impl Window {
    pub fn new(attrs: WindowAttributes) -> std::io::Result<Self> {
        pool(|| {
            let mut style = ns::WindowStyleMask::CLOSABLE
                | ns::WindowStyleMask::MINIATURIZABLE
                | ns::WindowStyleMask::RESIZABLE;
            if attrs.decorations != crate::config::window::Decorations::None {
                style |= ns::WindowStyleMask::TITLED;
            }
            if matches!(
                attrs.decorations,
                crate::config::window::Decorations::Transparent
                    | crate::config::window::Decorations::Buttonless
            ) {
                style |= ns::WindowStyleMask::FULL_SIZE_CONTENT_VIEW;
            }
            let rect = cg::Rect::new(0., 0., attrs.size.width as f64, attrs.size.height as f64);
            let mut window = TerminalWindow::alloc().init_with(|w| {
                w.init_with_content_rect_style_mask_backing_defer(
                    rect,
                    style,
                    ns::BackingStoreType::Buffered,
                    false,
                )
            });
            window.set_released_when_closed(false);
            window.set_title(&ns::String::with_str(&attrs.title));
            window.set_opaque(!attrs.transparent);
            window.set_background_color(&ns::Color::clear());
            window.set_accepts_mouse_moved(true);
            if attrs.decorations != crate::config::window::Decorations::Full {
                window.set_title_visibility(ns::WindowTitleVisibility::Hidden);
                window.set_titlebar_appears_transparent(true);
            }
            if attrs.decorations == crate::config::window::Decorations::Buttonless {
                for kind in 0..3 {
                    if let Some(mut button) = window.standard_button(kind) {
                        button.set_hidden(true);
                    }
                }
            }
            if let Some(id) = attrs.tabbing_id {
                window.set_tabbing_id(&ns::String::with_str(&id));
            }
            window.set_level(if attrs.level == WindowLevel::AlwaysOnTop {
                ns::WindowLevel::FLOATING
            } else {
                ns::WindowLevel::NORMAL
            });
            let id = WindowId(window.window_number() as u64);
            let delegate = unsafe { WindowDelegate::alloc_with(id).init_unchecked() };
            window.set_delegate(Some(delegate.as_ref()));
            let mut view = TerminalView::alloc_with(ViewState {
                id,
                marked: String::new(),
                selection: ns::Range::new(0, 0),
                ime_allowed: true,
                ime_rect: cg::Rect::new(0., 0., 1., 1.),
                key: None,
                option_as_alt: attrs.option_as_alt,
            })
            .init_with(|v| v.init_with_frame(rect));
            let mut layer = ca::MetalLayer::new();
            layer.set_opaque(!attrs.transparent);
            layer.set_maximum_drawable_count(3);
            view.set_wants_layer(true);
            view.set_layer(Some(&layer));
            view.set_autoresizing_mask(ns::AutoresizingMaskOpts(2 | 16));
            let mut blur = ns::VisualEffectView::new();
            blur.set_frame(rect);
            blur.set_blending_mode(ns::VisualEffectBlendingMode::BehindWindow);
            blur.set_material(ns::VisualEffectMaterial::UnderWindowBg);
            blur.set_state(ns::VisualEffectState::Active);
            blur.add_subview(&view);
            window.set_content_view(Some(&blur));
            window.make_first_responder(Some(&view));
            window.center();
            if let Some(position) = attrs.position {
                let mut frame = window.frame();
                frame.origin = cg::Point::new(position.x as f64, position.y as f64);
                window.set_frame_display(frame, false);
            }
            if attrs.maximized {
                window.zoom(None);
            }
            if attrs.fullscreen {
                window.toggle_full_screen(None);
            }
            let state = Rc::new(WindowState {
                window: RefCell::new(window),
                view: RefCell::new(view),
                layer: RefCell::new(layer),
                blur: RefCell::new(blur),
                attention: Cell::new(0),
                _delegate: delegate,
                id,
                size: Cell::new(PhysicalSize::new(0, 0)),
                scale: Cell::new(1.),
                focused: Cell::new(false),
                occluded: Cell::new(false),
                cursor_visible: Cell::new(true),
                simple_fullscreen: Cell::new(false),
                saved_frame: Cell::new(None),
            });
            state.update();
            WINDOWS.with_borrow_mut(|windows| windows.push(Rc::downgrade(&state)));
            let window = Self { state };
            window.set_blur(attrs.blur);
            window.set_theme(attrs.theme);
            Ok(window)
        })
    }
    pub fn metal_layer(&self) -> arc::R<ca::MetalLayer> {
        self.state.layer.borrow().clone()
    }
    pub fn id(&self) -> WindowId {
        self.state.id
    }
    pub fn inner_size(&self) -> PhysicalSize<u32> {
        self.state.size.get()
    }
    pub fn scale_factor(&self) -> f64 {
        self.state.scale.get()
    }
    pub fn request_inner_size(&self, size: PhysicalSize<u32>) -> Option<PhysicalSize<u32>> {
        let scale = self.scale_factor();
        self.state.window.borrow().set_content_size(cg::Size::new(
            size.width as f64 / scale,
            size.height as f64 / scale,
        ));
        self.state.update();
        Some(self.inner_size())
    }
    pub fn set_visible(&self, visible: bool) {
        if visible {
            self.state
                .window
                .borrow_mut()
                .make_key_and_order_front(None);
            ns::App::shared().activate_ignoring_other_apps(true);
        } else {
            self.state.window.borrow().order_out(None);
        }
    }
    pub fn set_title(&self, title: &str) {
        self.state
            .window
            .borrow_mut()
            .set_title(&ns::String::with_str(title));
    }
    pub fn request_redraw(&self) {
        emit(self.id(), WindowEvent::RedrawRequested);
    }
    pub fn set_cursor(&self, cursor: CursorIcon) {
        let cursor = match cursor {
            CursorIcon::Default => Cursor::arrow(),
            CursorIcon::Text => Cursor::text(),
            CursorIcon::Pointer => Cursor::pointer(),
        };
        cursor.set_cursor();
    }
    pub fn set_cursor_visible(&self, visible: bool) {
        if self.state.cursor_visible.replace(visible) != visible {
            if visible {
                Cursor::unhide_cursor();
            } else {
                Cursor::hide_cursor();
            }
        }
    }
    pub fn set_ime_allowed(&self, allowed: bool) {
        self.state.view.borrow_mut().inner_mut().ime_allowed = allowed;
        emit(
            self.id(),
            WindowEvent::Ime(if allowed { Ime::Enabled } else { Ime::Disabled }),
        );
    }
    pub fn set_ime_purpose(&self, _purpose: ImePurpose) {}
    pub fn set_ime_cursor_area(&self, position: PhysicalPosition<f64>, size: PhysicalSize<f64>) {
        let scale = self.scale_factor();
        self.state.view.borrow_mut().inner_mut().ime_rect = cg::Rect::new(
            position.x / scale,
            position.y / scale,
            size.width / scale,
            size.height / scale,
        );
    }
    pub fn set_transparent(&self, transparent: bool) {
        self.state.window.borrow_mut().set_opaque(!transparent);
        self.state.layer.borrow_mut().set_opaque(!transparent);
    }
    pub fn set_blur(&self, blur: bool) {
        self.state.blur.borrow_mut().set_state(if blur {
            ns::VisualEffectState::Active
        } else {
            ns::VisualEffectState::Inactive
        });
    }
    pub fn set_has_shadow(&self, shadow: bool) {
        self.state.window.borrow_mut().set_has_shadow(shadow);
    }
    pub fn set_maximized(&self, maximized: bool) {
        if self.is_maximized() != maximized {
            self.state.window.borrow().zoom(None);
        }
    }
    pub fn is_maximized(&self) -> bool {
        self.state.window.borrow().is_zoomed()
    }
    pub fn set_minimized(&self, minimized: bool) {
        if minimized {
            self.state.window.borrow().miniaturize(None);
        } else {
            self.state.window.borrow().deminiaturize(None);
        }
    }
    pub fn set_resize_increments(&self, increments: Option<PhysicalSize<f32>>) {
        let scale = self.scale_factor();
        let size = increments.map_or(cg::Size::new(1., 1.), |s| {
            cg::Size::new(s.width as f64 / scale, s.height as f64 / scale)
        });
        self.state.window.borrow().set_increments(size);
    }
    pub fn fullscreen(&self) -> Option<Fullscreen> {
        (self.state.window.borrow().style_mask().0 & ns::WindowStyleMask::FULL_SCREEN.0 != 0)
            .then_some(Fullscreen::Borderless(None))
    }
    pub fn set_fullscreen(&self, fullscreen: Option<Fullscreen>) {
        if self.fullscreen().is_some() != fullscreen.is_some() {
            self.state.window.borrow().toggle_full_screen(None);
        }
    }
    pub fn pre_present_notify(&self) {}
    pub fn set_theme(&self, theme: Option<Theme>) {
        let appearance = theme.and_then(|theme| {
            Appearance::named(if theme == Theme::Dark {
                ns::str!(c"NSAppearanceNameDarkAqua")
            } else {
                ns::str!(c"NSAppearanceNameAqua")
            })
        });
        self.state
            .window
            .borrow()
            .set_appearance(appearance.as_deref());
    }
    pub fn simple_fullscreen(&self) -> bool {
        self.state.simple_fullscreen.get()
    }
    pub fn set_simple_fullscreen(&self, fullscreen: bool) {
        if self.state.simple_fullscreen.replace(fullscreen) == fullscreen {
            return;
        }
        let mut window = self.state.window.borrow_mut();
        if fullscreen {
            self.state.saved_frame.set(Some(window.frame()));
            if let Some(screen) = window.screen() {
                window.set_frame_display(screen.frame(), true);
            }
        } else if let Some(frame) = self.state.saved_frame.take() {
            window.set_frame_display(frame, true);
        }
    }
    pub fn set_option_as_alt(&self, value: OptionAsAlt) {
        self.state.view.borrow_mut().inner_mut().option_as_alt = value;
    }
    pub fn current_monitor(&self) -> Option<MonitorHandle> {
        self.state
            .window
            .borrow()
            .screen()
            .map(|screen| MonitorHandle {
                refresh_rate: screen.fps().max(1) as u32 * 1000,
            })
    }
    pub fn num_tabs(&self) -> usize {
        self.state
            .window
            .borrow()
            .tabbed_windows()
            .map_or(1, |windows| windows.len())
    }
    pub fn select_tab_at_index(&self, index: usize) {
        if let Some(windows) = self.state.window.borrow().tabbed_windows() {
            if index < windows.len() {
                windows.get(index).unwrap().make_key_window();
            }
        }
    }
    pub fn select_next_tab(&self) {
        self.state.window.borrow().next_tab(None);
    }
    pub fn select_previous_tab(&self) {
        self.state.window.borrow().previous_tab(None);
    }
    pub fn tabbing_identifier(&self) -> String {
        self.state.window.borrow().tabbing_id().to_string()
    }
    pub fn request_user_attention(&self, attention: Option<UserAttentionType>) {
        let app = ns::App::shared();
        let old = self.state.attention.replace(0);
        if old != 0 {
            app.cancel_attention(old);
        }
        if attention.is_some() {
            self.state.attention.set(app.attention(0));
        }
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        self.set_cursor_visible(true);
        self.state.window.borrow().close();
    }
}
