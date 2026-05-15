use log::{debug, warn};

use crate::terminal::term::ClipboardType;
use copypasta::ClipboardProvider;

pub struct Clipboard {
    clipboard: Box<dyn ClipboardProvider>,
    selection: Option<Box<dyn ClipboardProvider>>,
}

impl Clipboard {
    /// Create a new nop clipboard (never fails, used on exit).
    pub fn new_nop() -> Self {
        Self {
            clipboard: Box::new(copypasta::nop_clipboard::NopClipboardContext::new().unwrap()),
            selection: None,
        }
    }

    /// Create a new clipboard.
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new clipboard on Unix (X11 or Wayland).
    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn new<T: 'static>(event_loop: &winit::event_loop::EventLoop<T>) -> Self {
        use winit::raw_window_handle::HasDisplayHandle;

        // Try Wayland first.
        match event_loop.display_handle() {
            Ok(handle) => {
                let raw = handle.as_raw();

                if let winit::raw_window_handle::RawDisplayHandle::Wayland(wayland) = raw {
                    let display = wayland.display.as_ptr();

                    unsafe {
                        let (primary, clipboard) =
                            copypasta::wayland_clipboard::create_clipboards_from_external(display);
                        return Self {
                            clipboard: Box::new(clipboard),
                            selection: Some(Box::new(primary)),
                        };
                    }
                }
            }
            Err(_) => {}
        }

        // Fall back to X11.
        Self::default()
    }
}

impl Default for Clipboard {
    fn default() -> Self {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            use copypasta::ClipboardContext;
            match ClipboardContext::new() {
                Ok(clipboard) => {
                    return Self {
                        clipboard: Box::new(clipboard),
                        selection: None,
                    };
                }
                Err(err) => {
                    warn!("Failed to initialize X11 clipboard: {err}");
                }
            }
        }

        // Fallback to nop clipboard (never fails).
        Self {
            clipboard: Box::new(copypasta::nop_clipboard::NopClipboardContext::new().unwrap()),
            selection: None,
        }
    }
}

impl Clipboard {
    pub fn store(&mut self, ty: ClipboardType, text: impl Into<String>) {
        let clipboard = match (ty, &mut self.selection) {
            (ClipboardType::Selection, Some(provider)) => provider,
            (ClipboardType::Selection, None) => return,
            _ => &mut self.clipboard,
        };

        clipboard.set_contents(text.into()).unwrap_or_else(|err| {
            warn!("Unable to store text in clipboard: {err}");
        });
    }

    pub fn load(&mut self, ty: ClipboardType) -> String {
        let clipboard = match (ty, &mut self.selection) {
            (ClipboardType::Selection, Some(provider)) => provider,
            _ => &mut self.clipboard,
        };

        match clipboard.get_contents() {
            Err(err) => {
                debug!("Unable to load text from clipboard: {err}");
                String::new()
            }
            Ok(text) => text,
        }
    }
}
