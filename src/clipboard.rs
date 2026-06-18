#[cfg_attr(not(windows), allow(unused_imports))]
use crate::terminal::term::ClipboardType;

#[cfg(windows)]
use std::{iter, mem, ptr, slice};

#[cfg(windows)]
use windows_sys::Win32::System::{
    DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
    },
    Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
    Ole::CF_UNICODETEXT,
};

#[cfg(all(unix, not(target_os = "macos")))]
use smithay_clipboard::Clipboard as WaylandClipboard;
#[cfg(all(unix, not(target_os = "macos")))]
use std::collections::HashMap;
#[cfg(all(unix, not(target_os = "macos")))]
use std::ffi::c_void;
#[cfg(all(unix, not(target_os = "macos")))]
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
#[cfg(all(unix, not(target_os = "macos")))]
use std::sync::{Arc, RwLock};
#[cfg(all(unix, not(target_os = "macos")))]
use std::thread;
#[cfg(all(unix, not(target_os = "macos")))]
use std::time::{Duration, Instant};
#[cfg(all(unix, not(target_os = "macos")))]
use winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};

#[cfg(all(unix, not(target_os = "macos")))]
use x11rb::connection::{Connection, RequestConnection};
#[cfg(all(unix, not(target_os = "macos")))]
use x11rb::protocol::Event;
#[cfg(all(unix, not(target_os = "macos")))]
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask,
    PropMode, Property, SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, Window, WindowClass,
};
#[cfg(all(unix, not(target_os = "macos")))]
use x11rb::rust_connection::RustConnection;
#[cfg(all(unix, not(target_os = "macos")))]
use x11rb::wrapper::ConnectionExt as _;
#[cfg(all(unix, not(target_os = "macos")))]
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME};

const INCR_CHUNK_SIZE: usize = 4000;
const POLL_DURATION_MS: u64 = 50;

#[cfg(all(unix, not(target_os = "macos")))]
#[derive(Clone, Debug)]
struct X11Atoms {
    primary: Atom,
    clipboard: Atom,
    property: Atom,
    targets: Atom,
    utf8_string: Atom,
    incr: Atom,
}

#[cfg(all(unix, not(target_os = "macos")))]
impl X11Atoms {
    fn intern_all(conn: &RustConnection) -> Result<Self, String> {
        let clipboard = conn
            .intern_atom(false, b"CLIPBOARD")
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?
            .atom;
        let property = conn
            .intern_atom(false, b"THIS_CLIPBOARD_OUT")
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?
            .atom;
        let targets = conn
            .intern_atom(false, b"TARGETS")
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?
            .atom;
        let utf8_string = conn
            .intern_atom(false, b"UTF8_STRING")
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?
            .atom;
        let incr = conn
            .intern_atom(false, b"INCR")
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?
            .atom;
        Ok(X11Atoms {
            primary: Atom::from(AtomEnum::PRIMARY),
            clipboard,
            property,
            targets,
            utf8_string,
            incr,
        })
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
struct X11Context {
    connection: RustConnection,
    #[allow(dead_code)]
    screen: usize,
    window: Window,
    atoms: X11Atoms,
}

#[cfg(all(unix, not(target_os = "macos")))]
impl X11Context {
    fn new() -> Result<Self, String> {
        let (connection, screen) = RustConnection::connect(None).map_err(|e| e.to_string())?;
        let window = connection.generate_id().map_err(|e| e.to_string())?;

        {
            let screen_obj = connection
                .setup()
                .roots
                .get(screen)
                .ok_or_else(|| "invalid screen".to_string())?;
            connection
                .create_window(
                    COPY_DEPTH_FROM_PARENT,
                    window,
                    screen_obj.root,
                    0,
                    0,
                    1,
                    1,
                    0,
                    WindowClass::INPUT_OUTPUT,
                    screen_obj.root_visual,
                    &CreateWindowAux::new()
                        .event_mask(EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE),
                )
                .map_err(|e| e.to_string())?
                .check()
                .map_err(|e| e.to_string())?;
        }

        let atoms = X11Atoms::intern_all(&connection)?;
        Ok(X11Context {
            connection,
            screen,
            window,
            atoms,
        })
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
type SetMap = Arc<RwLock<HashMap<Atom, (Atom, Vec<u8>)>>>;

#[cfg(all(unix, not(target_os = "macos")))]
struct X11State {
    getter: X11Context,
    setter: Arc<X11Context>,
    setmap: SetMap,
    send: Sender<Atom>,
    // thread handle kept to keep it alive
    _thread: Option<thread::JoinHandle<()>>,
}

#[cfg(all(unix, not(target_os = "macos")))]
impl X11State {
    fn new() -> Result<Self, String> {
        let getter = X11Context::new()?;
        let setter = Arc::new(X11Context::new()?);
        let setter2 = Arc::clone(&setter);
        let setmap: SetMap = Arc::new(RwLock::new(HashMap::new()));
        let setmap2 = Arc::clone(&setmap);

        let (sender, receiver) = mpsc::channel();
        let max_length = setter.connection.maximum_request_bytes();

        let setter_thread = thread::spawn(move || {
            run_setter(setter2, setmap2, max_length, receiver);
        });

        Ok(X11State {
            getter,
            setter,
            setmap,
            send: sender,
            _thread: Some(setter_thread),
        })
    }

    fn store(&self, selection: Atom, target: Atom, value: Vec<u8>) -> Result<(), String> {
        self.send.send(selection).map_err(|e| e.to_string())?;
        self.setmap
            .write()
            .map_err(|_| "lock".to_string())?
            .insert(selection, (target, value));

        self.setter
            .connection
            .set_selection_owner(self.setter.window, selection, CURRENT_TIME)
            .map_err(|e| e.to_string())?
            .check()
            .map_err(|e| e.to_string())?;

        let owner = self
            .setter
            .connection
            .get_selection_owner(selection)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?
            .owner;
        if owner == self.setter.window {
            Ok(())
        } else {
            Err("failed to own selection".to_string())
        }
    }

    fn load(
        &self,
        selection: Atom,
        target: Atom,
        property: Atom,
        timeout: Duration,
    ) -> Result<Vec<u8>, String> {
        let mut buf = Vec::new();
        let _start = Instant::now();

        let cookie = self
            .getter
            .connection
            .convert_selection(
                self.getter.window,
                selection,
                target,
                property,
                CURRENT_TIME,
            )
            .map_err(|e| e.to_string())?;
        let seq = cookie.sequence_number();
        cookie.check().map_err(|e| e.to_string())?;

        self.process_event(
            &mut buf,
            selection,
            target,
            property,
            Some(timeout),
            false,
            seq,
        )?;

        self.getter
            .connection
            .delete_property(self.getter.window, property)
            .map_err(|e| e.to_string())?
            .check()
            .map_err(|e| e.to_string())?;

        Ok(buf)
    }

    #[allow(clippy::too_many_arguments)]
    fn process_event(
        &self,
        buff: &mut Vec<u8>,
        selection: Atom,
        target: Atom,
        property: Atom,
        timeout: Option<Duration>,
        _use_xfixes: bool,
        _sequence_number: u64,
    ) -> Result<(), String> {
        let _ = (_use_xfixes, _sequence_number);
        let mut is_incr = false;
        let start_time = timeout.map(|_| Instant::now());

        loop {
            if let (Some(t), Some(st)) = (timeout, start_time)
                && st.elapsed() >= t
            {
                return Err("timeout".to_string());
            }

            let event = match self.getter.connection.poll_for_event() {
                Ok(Some(e)) => e,
                Ok(None) => {
                    thread::park_timeout(Duration::from_millis(POLL_DURATION_MS));
                    continue;
                }
                Err(_) => return Err("event error".to_string()),
            };

            // simple sequence check skipped for brevity; x11rb seq is different
            match event {
                Event::SelectionNotify(event) => {
                    if event.selection != selection {
                        continue;
                    }
                    if event.property == Atom::from(AtomEnum::NONE) {
                        break;
                    }
                    let reply = self
                        .getter
                        .connection
                        .get_property(
                            false,
                            self.getter.window,
                            event.property,
                            AtomEnum::NONE,
                            buff.len() as u32,
                            u32::MAX,
                        )
                        .map_err(|e| e.to_string())?
                        .reply()
                        .map_err(|e| e.to_string())?;

                    if reply.type_ == self.getter.atoms.incr {
                        if let Some(mut it) = reply.value32()
                            && let Some(sz) = it.next()
                        {
                            buff.reserve(sz as usize);
                        }
                        self.getter
                            .connection
                            .delete_property(self.getter.window, property)
                            .map_err(|e| e.to_string())?
                            .check()
                            .map_err(|e| e.to_string())?;
                        is_incr = true;
                        continue;
                    } else if reply.type_ != target {
                        return Err("unexpected type".to_string());
                    }
                    buff.extend_from_slice(&reply.value);
                    break;
                }
                Event::PropertyNotify(event) if is_incr => {
                    if event.state != Property::NEW_VALUE {
                        continue;
                    }
                    let cookie = self
                        .getter
                        .connection
                        .get_property(false, self.getter.window, property, AtomEnum::NONE, 0, 0)
                        .map_err(|e| e.to_string())?;
                    let length = cookie.reply().map_err(|e| e.to_string())?.bytes_after;
                    let reply = self
                        .getter
                        .connection
                        .get_property(
                            true,
                            self.getter.window,
                            property,
                            AtomEnum::NONE,
                            0,
                            length,
                        )
                        .map_err(|e| e.to_string())?
                        .reply()
                        .map_err(|e| e.to_string())?;
                    if reply.type_ != target {
                        continue;
                    }
                    if !reply.value.is_empty() {
                        buff.extend_from_slice(&reply.value);
                    } else {
                        break;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn run_setter(
    context: Arc<X11Context>,
    setmap: SetMap,
    max_length: usize,
    receiver: Receiver<Atom>,
) {
    let mut incr_map: HashMap<Atom, Atom> = HashMap::new();
    let mut state_map: HashMap<Atom, IncrState> = HashMap::new();

    loop {
        let evt = match context.connection.poll_for_event() {
            Ok(Some(e)) => e,
            Ok(None) => {
                thread::park_timeout(Duration::from_millis(POLL_DURATION_MS));
                continue;
            }
            Err(_) => return,
        };

        // drain receiver
        loop {
            match receiver.try_recv() {
                Ok(sel) => {
                    if let Some(p) = incr_map.remove(&sel) {
                        state_map.remove(&p);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if state_map.is_empty() {
                        return;
                    }
                }
            }
        }

        match evt {
            Event::SelectionRequest(event) => {
                let read_map = match setmap.read() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let &(tgt, ref value) = match read_map.get(&event.selection) {
                    Some(v) => v,
                    None => continue,
                };

                if event.target == context.atoms.targets {
                    let _ = context.connection.change_property32(
                        PropMode::REPLACE,
                        event.requestor,
                        event.property,
                        Atom::from(AtomEnum::ATOM),
                        &[context.atoms.targets, tgt],
                    );
                } else if value.len() < max_length.saturating_sub(24) {
                    let _ = context.connection.change_property8(
                        PropMode::REPLACE,
                        event.requestor,
                        event.property,
                        tgt,
                        value,
                    );
                } else {
                    let _ = context.connection.change_window_attributes(
                        event.requestor,
                        &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
                    );
                    let _ = context.connection.change_property32(
                        PropMode::REPLACE,
                        event.requestor,
                        event.property,
                        context.atoms.incr,
                        &[],
                    );
                    incr_map.insert(event.selection, event.property);
                    state_map.insert(
                        event.property,
                        IncrState {
                            selection: event.selection,
                            requestor: event.requestor,
                            property: event.property,
                            pos: 0,
                        },
                    );
                }
                let _ = context.connection.send_event(
                    false,
                    event.requestor,
                    EventMask::default(),
                    SelectionNotifyEvent {
                        response_type: SELECTION_NOTIFY_EVENT,
                        sequence: 0,
                        time: event.time,
                        requestor: event.requestor,
                        selection: event.selection,
                        target: event.target,
                        property: event.property,
                    },
                );
                let _ = context.connection.flush();
            }
            Event::PropertyNotify(event) => {
                if event.state != Property::DELETE {
                    continue;
                }
                let is_end = {
                    let st = match state_map.get_mut(&event.atom) {
                        Some(s) => s,
                        None => continue,
                    };
                    let read_set = match setmap.read() {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    let &(tgt, ref val) = match read_set.get(&st.selection) {
                        Some(v) => v,
                        None => continue,
                    };
                    let len = std::cmp::min(INCR_CHUNK_SIZE, val.len() - st.pos);
                    let _ = context.connection.change_property8(
                        PropMode::REPLACE,
                        st.requestor,
                        st.property,
                        tgt,
                        &val[st.pos..][..len],
                    );
                    st.pos += len;
                    len == 0
                };
                if is_end {
                    state_map.remove(&event.atom);
                }
                let _ = context.connection.flush();
            }
            Event::SelectionClear(event) => {
                if let Some(p) = incr_map.remove(&event.selection) {
                    state_map.remove(&p);
                }
                if let Ok(mut w) = setmap.write() {
                    w.remove(&event.selection);
                }
            }
            _ => {}
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
#[derive(Clone, Copy)]
struct IncrState {
    selection: Atom,
    requestor: Window,
    property: Atom,
    pos: usize,
}

pub struct Clipboard {
    #[cfg(all(unix, not(target_os = "macos")))]
    wayland: Option<WaylandClipboard>,
    #[cfg(all(unix, not(target_os = "macos")))]
    #[allow(dead_code)]
    wayland_display: Option<*mut c_void>,
    #[cfg(all(unix, not(target_os = "macos")))]
    x11: Option<X11State>,
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    _priv: (),
}

impl Clipboard {
    pub fn new_nop() -> Self {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            Self {
                wayland: None,
                wayland_display: None,
                x11: None,
            }
        }
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        {
            Self { _priv: () }
        }
    }

    #[cfg(not(all(unix, not(target_os = "macos"))))]
    pub fn new() -> Self {
        Self { _priv: () }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn new<T: 'static>(event_loop: &winit::event_loop::EventLoop<T>) -> Self {
        if let Ok(handle) = event_loop.display_handle() {
            match handle.as_raw() {
                RawDisplayHandle::Wayland(wh) => {
                    let p = wh.display.as_ptr();
                    let c = unsafe { WaylandClipboard::new(p) };
                    return Self {
                        wayland: Some(c),
                        wayland_display: Some(p),
                        x11: None,
                    };
                }
                RawDisplayHandle::Xlib(_) | RawDisplayHandle::Xcb(_) => {
                    if let Ok(st) = X11State::new() {
                        return Self {
                            wayland: None,
                            wayland_display: None,
                            x11: Some(st),
                        };
                    }
                }
                _ => {}
            }
        }
        if let Ok(st) = X11State::new() {
            return Self {
                wayland: None,
                wayland_display: None,
                x11: Some(st),
            };
        }
        Self {
            wayland: None,
            wayland_display: None,
            x11: None,
        }
    }

    pub fn store(&mut self, ty: ClipboardType, text: impl Into<String>) {
        #[cfg(windows)]
        {
            if ty == ClipboardType::Selection {
                return;
            }
            if let Err(err) = windows_set_clipboard(&text.into()) {
                log::warn!("Unable to store text in clipboard: {err}");
            }
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let text = text.into();
            if ty == ClipboardType::Selection {
                if let Some(ref w) = self.wayland {
                    w.store_primary(text);
                } else if let Some(ref x) = self.x11 {
                    let _ = x.store(
                        x.getter.atoms.primary,
                        x.getter.atoms.utf8_string,
                        text.into_bytes(),
                    );
                }
                return;
            }
            if let Some(ref w) = self.wayland {
                w.store(text);
            } else if let Some(ref x) = self.x11 {
                let _ = x.store(
                    x.getter.atoms.clipboard,
                    x.getter.atoms.utf8_string,
                    text.into_bytes(),
                );
            }
        }
    }

    pub fn load(&mut self, ty: ClipboardType) -> String {
        #[cfg(windows)]
        {
            if ty == ClipboardType::Selection {
                return String::new();
            }
            match windows_get_clipboard() {
                Ok(t) => return t,
                Err(e) => {
                    log::debug!("Unable to load text from clipboard: {e}");
                    return String::new();
                }
            }
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            if ty == ClipboardType::Selection {
                if let Some(ref w) = self.wayland {
                    if let Ok(s) = w.load_primary() {
                        return s;
                    }
                } else if let Some(ref x) = self.x11
                    && let Ok(v) = x.load(
                        x.getter.atoms.primary,
                        x.getter.atoms.utf8_string,
                        x.getter.atoms.property,
                        Duration::from_secs(3),
                    )
                    && let Ok(s) = String::from_utf8(v)
                {
                    return s;
                }
                return String::new();
            }
            if let Some(ref w) = self.wayland {
                if let Ok(s) = w.load() {
                    return s;
                }
            } else if let Some(ref x) = self.x11
                && let Ok(v) = x.load(
                    x.getter.atoms.clipboard,
                    x.getter.atoms.utf8_string,
                    x.getter.atoms.property,
                    Duration::from_secs(3),
                )
                && let Ok(s) = String::from_utf8(v)
            {
                return s;
            }
            String::new()
        }
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        {
            let _ = ty;
            String::new()
        }
    }
}

impl Default for Clipboard {
    fn default() -> Self {
        Self::new_nop()
    }
}

#[cfg(windows)]
fn windows_set_clipboard(text: &str) -> Result<(), String> {
    let wide: Vec<u16> = text.encode_utf16().chain(iter::once(0)).collect();
    let size = wide.len() * mem::size_of::<u16>();
    let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, size) };
    if handle.is_null() {
        return Err("GlobalAlloc failed".to_string());
    }
    let data = unsafe { GlobalLock(handle) };
    if data.is_null() {
        return Err("GlobalLock failed".to_string());
    }
    unsafe {
        ptr::copy_nonoverlapping(wide.as_ptr(), data.cast::<u16>(), wide.len());
        GlobalUnlock(handle);
    }
    unsafe {
        if OpenClipboard(ptr::null_mut()) == 0 {
            return Err("OpenClipboard failed".to_string());
        }
        if EmptyClipboard() == 0 {
            CloseClipboard();
            return Err("EmptyClipboard failed".to_string());
        }
        if SetClipboardData(CF_UNICODETEXT as u32, handle).is_null() {
            CloseClipboard();
            return Err("SetClipboardData failed".to_string());
        }
        CloseClipboard();
    }
    Ok(())
}

#[cfg(windows)]
fn windows_get_clipboard() -> Result<String, String> {
    unsafe {
        if OpenClipboard(ptr::null_mut()) == 0 {
            return Err("OpenClipboard failed".to_string());
        }
        let handle = GetClipboardData(CF_UNICODETEXT as u32);
        if handle.is_null() {
            CloseClipboard();
            return Err("GetClipboardData failed".to_string());
        }
        let data = GlobalLock(handle);
        if data.is_null() {
            CloseClipboard();
            return Err("GlobalLock failed".to_string());
        }
        let mut len = 0;
        let p = data.cast::<u16>();
        while *p.add(len) != 0 {
            len += 1;
        }
        let s = String::from_utf16_lossy(slice::from_raw_parts(p, len));
        GlobalUnlock(handle);
        CloseClipboard();
        Ok(s)
    }
}
