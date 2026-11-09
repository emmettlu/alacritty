use std::fmt::{self, Display, Formatter};
use std::rc::Rc;

use bitflags::bitflags;

use crate::platform::dpi::{PhysicalPosition, PhysicalSize};
use crate::platform::event_loop::ActiveEventLoop;
use crate::platform::monitor::MonitorHandle;
use crate::platform::window::{
    CursorIcon, Fullscreen, ImePurpose, OptionAsAlt, Theme, UserAttentionType,
    Window as NativeWindow, WindowAttributes, WindowId,
};

use crate::terminal::index::Point;

use crate::cli::WindowOptions;
use crate::config::UiConfig;
use crate::config::window::Identity;
use crate::display::SizeInfo;

/// Window errors.
#[derive(Debug)]
pub enum Error {
    /// Error creating the window.
    WindowCreation(std::io::Error),
}

/// Result of fallible operations concerning a Window.
type Result<T> = std::result::Result<T, Error>;

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::WindowCreation(err) => err.source(),
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Error::WindowCreation(err) => write!(f, "Error creating window; {err}"),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(val: std::io::Error) -> Self {
        Error::WindowCreation(val)
    }
}

/// A window which can be used for displaying the terminal.
///
/// Wraps the underlying windowing library to provide a stable API in Alacritty.
pub struct Window {
    /// Flag tracking that we have a frame we can draw.
    pub has_frame: bool,

    /// Cached scale factor for quickly scaling pixel sizes.
    pub scale_factor: f64,

    /// Flag indicating whether redraw was requested.
    pub requested_redraw: bool,

    /// Hold the window when terminal exits.
    pub hold: bool,

    window: Rc<NativeWindow>,

    /// Current window title.
    title: String,

    is_x11: bool,
    current_mouse_cursor: CursorIcon,
    mouse_visible: bool,
    ime_inhibitor: ImeInhibitor,
}

impl Window {
    /// Create a new window.
    ///
    /// This creates a window and fully initializes a window.
    pub fn new(
        event_loop: &ActiveEventLoop,
        config: &UiConfig,
        identity: &Identity,
        options: &mut WindowOptions,
    ) -> Result<Window> {
        let identity = identity.clone();
        let window_attributes = WindowAttributes {
            title: identity.title.clone(),
            position: config
                .window
                .position
                .map(|p| PhysicalPosition::new(p.x, p.y)),
            size: PhysicalSize::new(800, 600),
            decorations: config.window.decorations,
            transparent: config.window_opacity() < 1.0,
            blur: config.window.blur,
            maximized: config.window.maximized(),
            fullscreen: config.window.fullscreen().is_some(),
            level: config.window.level.into(),
            theme: config.window.theme(),
            option_as_alt: config.window.native_option_as_alt(),
            tabbing_id: {
                #[cfg(target_os = "macos")]
                {
                    options.window_tabbing_id.take()
                }
                #[cfg(not(target_os = "macos"))]
                {
                    None
                }
            },
            class: identity.class.clone(),
            #[cfg(all(unix, not(target_os = "macos")))]
            embed: config.window.embed,
        };
        let window = Rc::new(event_loop.create_window(window_attributes)?);

        // Text cursor.
        let current_mouse_cursor = CursorIcon::Text;
        window.set_cursor(current_mouse_cursor);

        // Enable IME.
        window.set_ime_allowed(true);
        window.set_ime_purpose(ImePurpose::Terminal);

        // Set initial transparency hint.
        window.set_transparent(config.window_opacity() < 1.);

        let scale_factor = window.scale_factor();
        log::info!("Window scale factor: {scale_factor}");
        let is_x11 = cfg!(all(unix, not(target_os = "macos")));

        Ok(Self {
            hold: options.terminal_options.hold,
            requested_redraw: false,
            title: identity.title,
            current_mouse_cursor,
            mouse_visible: true,
            has_frame: true,
            scale_factor,
            window,
            is_x11,
            ime_inhibitor: Default::default(),
        })
    }

    #[inline]
    pub fn request_inner_size(&self, size: PhysicalSize<u32>) {
        let _ = self.window.request_inner_size(size);
    }

    #[inline]
    pub fn inner_size(&self) -> PhysicalSize<u32> {
        self.window.inner_size()
    }

    #[inline]
    pub fn set_visible(&self, visibility: bool) {
        self.window.set_visible(visibility);
    }

    /// Set the window title.
    #[inline]
    pub fn set_title(&mut self, title: String) {
        self.title = title;
        self.window.set_title(&self.title);
    }

    /// Get the window title.
    #[cfg(unix)]
    #[inline]
    pub fn title(&self) -> &str {
        &self.title
    }

    #[inline]
    pub fn request_redraw(&mut self) {
        if !self.requested_redraw {
            self.requested_redraw = true;
            self.window.request_redraw();
        }
    }

    #[inline]
    pub fn set_mouse_cursor(&mut self, cursor: CursorIcon) {
        if cursor != self.current_mouse_cursor {
            self.current_mouse_cursor = cursor;
            self.window.set_cursor(cursor);
        }
    }

    /// Set mouse cursor visible.
    pub fn set_mouse_visible(&mut self, visible: bool) {
        if visible != self.mouse_visible {
            self.mouse_visible = visible;
            self.window.set_cursor_visible(visible);
        }
    }

    #[inline]
    pub fn mouse_visible(&self) -> bool {
        self.mouse_visible
    }

    pub fn set_urgent(&self, is_urgent: bool) {
        let attention = if is_urgent {
            Some(UserAttentionType::Critical)
        } else {
            None
        };

        self.window.request_user_attention(attention);
    }

    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    /// Clone the native window handle for surface creation.
    pub fn surface_target(&self) -> Rc<NativeWindow> {
        Rc::clone(&self.window)
    }

    #[cfg(unix)]
    pub fn set_transparent(&self, transparent: bool) {
        self.window.set_transparent(transparent);
    }

    #[cfg(unix)]
    pub fn set_blur(&self, blur: bool) {
        self.window.set_blur(blur);
    }

    /// Disable macOS window shadows for transparent windows.
    #[cfg(target_os = "macos")]
    pub fn set_has_shadow(&self, has_shadow: bool) {
        self.window.set_has_shadow(has_shadow);
    }

    pub fn set_maximized(&self, maximized: bool) {
        self.window.set_maximized(maximized);
    }

    pub fn set_minimized(&self, minimized: bool) {
        self.window.set_minimized(minimized);
    }

    pub fn set_resize_increments(&self, increments: Option<PhysicalSize<f32>>) {
        self.window.set_resize_increments(increments);
    }

    /// Toggle the window's fullscreen state.
    pub fn toggle_fullscreen(&self) {
        self.set_fullscreen(self.window.fullscreen().is_none());
    }

    /// Toggle the window's maximized state.
    pub fn toggle_maximized(&self) {
        self.set_maximized(!self.window.is_maximized());
    }

    /// Inform windowing system about presenting to the window.
    ///
    /// Should be called right before presenting to the window with e.g. `eglSwapBuffers`.
    pub fn pre_present_notify(&self) {
        self.window.pre_present_notify();
    }

    #[cfg(unix)]
    pub fn set_theme(&self, theme: Option<Theme>) {
        self.window.set_theme(theme);
    }

    #[cfg(target_os = "macos")]
    pub fn toggle_simple_fullscreen(&self) {
        self.window
            .set_simple_fullscreen(!self.window.simple_fullscreen());
    }

    #[cfg(target_os = "macos")]
    pub fn set_option_as_alt(&self, option_as_alt: OptionAsAlt) {
        self.window.set_option_as_alt(option_as_alt);
    }

    pub fn set_fullscreen(&self, fullscreen: bool) {
        if fullscreen {
            self.window
                .set_fullscreen(Some(Fullscreen::Borderless(None)));
        } else {
            self.window.set_fullscreen(None);
        }
    }

    pub fn current_monitor(&self) -> Option<MonitorHandle> {
        self.window.current_monitor()
    }

    /// Select tab at the given `index`.
    #[cfg(target_os = "macos")]
    pub fn select_tab_at_index(&self, index: usize) {
        self.window.select_tab_at_index(index);
    }

    /// Select the last tab.
    #[cfg(target_os = "macos")]
    pub fn select_last_tab(&self) {
        if let Some(index) = self.window.num_tabs().checked_sub(1) {
            self.window.select_tab_at_index(index);
        }
    }

    /// Select next tab.
    #[cfg(target_os = "macos")]
    pub fn select_next_tab(&self) {
        self.window.select_next_tab();
    }

    /// Select previous tab.
    #[cfg(target_os = "macos")]
    pub fn select_previous_tab(&self) {
        self.window.select_previous_tab();
    }

    #[cfg(target_os = "macos")]
    pub fn tabbing_id(&self) -> String {
        self.window.tabbing_identifier()
    }

    /// Set IME inhibitor state and disable IME while any are present.
    ///
    /// IME is re-enabled once all inhibitors are unset.
    pub fn set_ime_inhibitor(&mut self, inhibitor: ImeInhibitor, inhibit: bool) {
        if self.ime_inhibitor.contains(inhibitor) != inhibit {
            self.ime_inhibitor.set(inhibitor, inhibit);
            self.window.set_ime_allowed(self.ime_inhibitor.is_empty());
        }
    }

    /// Adjust the IME editor position according to the new location of the cursor.
    pub fn update_ime_position(&self, point: Point<usize>, size: &SizeInfo) {
        // NOTE: X11 doesn't support cursor area, so we need to offset manually to not obscure
        // the text.
        let offset = if self.is_x11 { 1 } else { 0 };
        let nspot_x = f64::from(size.padding_x() + point.column.0 as f32 * size.cell_width());
        let nspot_y =
            f64::from(size.padding_y() + (point.line + offset) as f32 * size.cell_height());

        // NOTE: some compositors don't like excluding too much and try to render popup at the
        // bottom right corner of the provided area, so exclude just the full-width char to not
        // obscure the cursor and not render popup at the end of the window.
        let width = size.cell_width() as f64 * 2.;
        let height = size.cell_height as f64;

        self.window.set_ime_cursor_area(
            PhysicalPosition::new(nspot_x, nspot_y),
            PhysicalSize::new(width, height),
        );
    }
}

bitflags! {
    /// IME inhibition sources.
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct ImeInhibitor: u8 {
        const FOCUS = 1;
        const TOUCH = 1 << 1;
        const VI    = 1 << 2;
    }
}
