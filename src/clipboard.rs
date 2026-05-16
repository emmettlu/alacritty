use log::{debug, warn};

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

pub struct Clipboard;

impl Clipboard {
    /// Create a new nop clipboard (never fails, used on exit).
    pub fn new_nop() -> Self {
        Self
    }

    /// Create a new clipboard.
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    pub fn new() -> Self {
        Self
    }

    /// Create a new clipboard on Unix (currently a nop implementation).
    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn new<T: 'static>(_: &winit::event_loop::EventLoop<T>) -> Self {
        Self
    }

    pub fn store(&mut self, ty: ClipboardType, text: impl Into<String>) {
        #[cfg(windows)]
        {
            // Windows only has a regular clipboard, no primary selection.
            if ty == ClipboardType::Selection {
                return;
            }

            if let Err(err) = windows_set_clipboard(&text.into()) {
                warn!("Unable to store text in clipboard: {err}");
            }
        }

        #[cfg(not(windows))]
        {
            let _ = (ty, text.into());
        }
    }

    pub fn load(&mut self, ty: ClipboardType) -> String {
        #[cfg(windows)]
        {
            // Windows only has a regular clipboard, no primary selection.
            if ty == ClipboardType::Selection {
                return String::new();
            }

            match windows_get_clipboard() {
                Ok(text) => text,
                Err(err) => {
                    debug!("Unable to load text from clipboard: {err}");
                    String::new()
                }
            }
        }

        #[cfg(not(windows))]
        {
            let _ = ty;
            String::new()
        }
    }
}

impl Default for Clipboard {
    fn default() -> Self {
        Self
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

        // After SetClipboardData succeeds, ownership of handle belongs to the system.
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
        let ptr = data.cast::<u16>();
        while *ptr.add(len) != 0 {
            len += 1;
        }

        let text = String::from_utf16_lossy(slice::from_raw_parts(ptr, len));

        GlobalUnlock(handle);
        CloseClipboard();

        Ok(text)
    }
}
