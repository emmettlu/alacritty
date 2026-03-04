#[cfg(not(windows))]
use std::borrow::Cow;
#[cfg(not(windows))]
use std::collections::HashSet;
#[cfg(not(windows))]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(not(windows))]
use std::{fmt, ptr};

#[cfg(not(windows))]
use ahash::RandomState;
#[cfg(not(windows))]
use crossfont::Metrics;
#[cfg(not(windows))]
use log::{LevelFilter, debug, info};
#[cfg(not(windows))]
use unicode_width::UnicodeWidthChar;

#[cfg(not(windows))]
use alacritty_terminal::index::Point;
#[cfg(not(windows))]
use alacritty_terminal::term::cell::Flags;

#[cfg(not(windows))]
use crate::display::SizeInfo;
#[cfg(not(windows))]
use crate::display::color::Rgb;
#[cfg(not(windows))]
use crate::display::content::RenderableCell;
#[cfg(not(windows))]
use crate::renderer::rects::RenderRect;

use std::fmt;

// OpenGL 模块 - 仅在非 Windows 平台时使用
#[cfg(not(windows))]
use std::ffi::{CStr, CString};
#[cfg(not(windows))]
use std::sync::OnceLock;

#[cfg(not(windows))]
use glutin::context::{ContextApi, GlContext, PossiblyCurrentContext};
#[cfg(not(windows))]
use glutin::display::{GetGlDisplay, GlDisplay};

#[cfg(not(windows))]
use crate::config::debug::RendererPreference;
#[cfg(not(windows))]
use crate::gl;
#[cfg(not(windows))]
use crate::renderer::shader::ShaderError;

#[cfg(not(windows))]
pub mod platform;
pub mod rects;
#[cfg(not(windows))]
mod shader;
pub(crate) mod text;

#[cfg(windows)]
pub mod wgpu_backend;

// 条件导出: Windows 上使用 wgpu 后端的 GlyphCache
#[cfg(windows)]
pub use wgpu_backend::GlyphCache;

// 非 Windows 上使用 OpenGL 后端的 GlyphCache / LoaderApi
#[cfg(not(windows))]
pub use text::{GlyphCache, LoaderApi};

#[cfg(not(windows))]
use shader::ShaderVersion;
#[cfg(not(windows))]
use text::{Gles2Renderer, Glsl3Renderer, TextRenderer};

/// OpenGL 函数是否已加载.
#[cfg(not(windows))]
pub static GL_FUNS_LOADED: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
pub enum Error {
    /// 着色器错误.
    #[cfg(not(windows))]
    Shader(ShaderError),

    /// 其他错误.
    Other(String),
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            #[cfg(not(windows))]
            Error::Shader(err) => err.source(),
            Error::Other(_) => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(not(windows))]
            Error::Shader(err) => {
                write!(f, "There was an error initializing the shaders: {err}")
            },
            Error::Other(err) => {
                write!(f, "{err}")
            },
        }
    }
}

#[cfg(not(windows))]
impl From<ShaderError> for Error {
    fn from(val: ShaderError) -> Self {
        Error::Shader(val)
    }
}

impl From<String> for Error {
    fn from(val: String) -> Self {
        Error::Other(val)
    }
}

// ============================================================
// 以下为 OpenGL 渲染器 (非 Windows 平台)
// ============================================================

// ============================================================
// OpenGL 渲染器 (非 Windows 平台)
// ============================================================

#[cfg(not(windows))]
#[derive(Debug)]
enum TextRendererProvider {
    Gles2(Gles2Renderer),
    Glsl3(Glsl3Renderer),
}

#[cfg(not(windows))]
#[derive(Debug)]
pub struct Renderer {
    text_renderer: TextRendererProvider,
    rect_renderer: rects::RectRenderer,
    robustness: bool,
}

#[cfg(not(windows))]
/// gl::GetString 的包装, 带错误检查和报告.
fn gl_get_string(
    string_id: gl::types::GLenum,
    description: &str,
) -> Result<Cow<'static, str>, Error> {
    unsafe {
        let string_ptr = gl::GetString(string_id);
        match gl::GetError() {
            gl::NO_ERROR if !string_ptr.is_null() => {
                Ok(CStr::from_ptr(string_ptr as *const _).to_string_lossy())
            },
            gl::INVALID_ENUM => {
                Err(format!("OpenGL error requesting {description}: invalid enum").into())
            },
            error_id => Err(format!("OpenGL error {error_id} requesting {description}").into()),
        }
    }
}

#[cfg(not(windows))]
impl Renderer {
    /// 创建新的渲染器.
    ///
    /// 根据 GPU 支持的 OpenGL 版本自动选择 GLES2 或 GLSL3 渲染器.
    pub fn new(
        context: &PossiblyCurrentContext,
        renderer_preference: Option<RendererPreference>,
    ) -> Result<Self, Error> {
        // 每个实例需要加载一次 OpenGL 函数, 但只在使 context 成为当前状态之后
        // (由于 WGL 的限制).
        if !GL_FUNS_LOADED.swap(true, Ordering::Relaxed) {
            let gl_display = context.display();
            gl::load_with(|symbol| {
                let symbol = CString::new(symbol).unwrap();
                gl_display.get_proc_address(symbol.as_c_str()).cast()
            });
        }

        let shader_version = gl_get_string(gl::SHADING_LANGUAGE_VERSION, "shader version")?;
        let gl_version = gl_get_string(gl::VERSION, "OpenGL version")?;
        let renderer = gl_get_string(gl::RENDERER, "renderer version")?;

        info!("Running on {renderer}");
        info!("OpenGL version {gl_version}, shader_version {shader_version}");

        // 检查是否支持 robustness.
        let robustness = Self::supports_robustness();

        let is_gles_context = matches!(context.context_api(), ContextApi::Gles(_));

        // 使用配置选项强制使用特定的渲染器配置.
        let (use_glsl3, allow_dsb) = match renderer_preference {
            Some(RendererPreference::Glsl3) => (true, true),
            Some(RendererPreference::Gles2) => (false, true),
            Some(RendererPreference::Gles2Pure) => (false, false),
            None => (shader_version.as_ref() >= "3.3" && !is_gles_context, true),
        };

        let (text_renderer, rect_renderer) = if use_glsl3 {
            let text_renderer = TextRendererProvider::Glsl3(Glsl3Renderer::new()?);
            let rect_renderer = rects::RectRenderer::new(ShaderVersion::Glsl3)?;
            (text_renderer, rect_renderer)
        } else {
            let text_renderer =
                TextRendererProvider::Gles2(Gles2Renderer::new(allow_dsb, is_gles_context)?);
            let rect_renderer = rects::RectRenderer::new(ShaderVersion::Gles2)?;
            (text_renderer, rect_renderer)
        };

        // 为 OpenGL 启用调试日志.
        if log::max_level() >= LevelFilter::Debug && GlExtensions::contains("GL_KHR_debug") {
            debug!("Enabled debug logging for OpenGL");
            unsafe {
                gl::Enable(gl::DEBUG_OUTPUT);
                gl::Enable(gl::DEBUG_OUTPUT_SYNCHRONOUS);
                gl::DebugMessageCallback(Some(gl_debug_log), ptr::null_mut());
            }
        }

        Ok(Self { text_renderer, rect_renderer, robustness })
    }

    pub fn draw_cells<I: Iterator<Item = RenderableCell>>(
        &mut self,
        size_info: &SizeInfo,
        glyph_cache: &mut GlyphCache,
        cells: I,
    ) {
        match &mut self.text_renderer {
            TextRendererProvider::Gles2(renderer) => {
                renderer.draw_cells(size_info, glyph_cache, cells)
            },
            TextRendererProvider::Glsl3(renderer) => {
                renderer.draw_cells(size_info, glyph_cache, cells)
            },
        }
    }

    /// 在变化的位置绘制字符串. 用于打印渲染计时器, 警告和错误.
    pub fn draw_string(
        &mut self,
        point: Point<usize>,
        fg: Rgb,
        bg: Rgb,
        string_chars: impl Iterator<Item = char>,
        size_info: &SizeInfo,
        glyph_cache: &mut GlyphCache,
    ) {
        let mut wide_char_spacer = false;
        let cells = string_chars.enumerate().filter_map(|(i, character)| {
            let flags = if wide_char_spacer {
                wide_char_spacer = false;
                return None;
            } else if character.width() == Some(2) {
                // spacer 总是跟在宽字符后面.
                wide_char_spacer = true;
                Flags::WIDE_CHAR
            } else {
                Flags::empty()
            };

            Some(RenderableCell {
                point: Point::new(point.line, point.column + i),
                character,
                extra: None,
                flags,
                bg_alpha: 1.0,
                fg,
                bg,
                underline: fg,
            })
        });

        self.draw_cells(size_info, glyph_cache, cells);
    }

    pub fn with_loader<F, T>(&mut self, func: F) -> T
    where
        F: FnOnce(LoaderApi<'_>) -> T,
    {
        match &mut self.text_renderer {
            TextRendererProvider::Gles2(renderer) => renderer.with_loader(func),
            TextRendererProvider::Glsl3(renderer) => renderer.with_loader(func),
        }
    }

    /// 同时绘制所有矩形, 以防止过多的程序切换.
    pub fn draw_rects(&mut self, size_info: &SizeInfo, metrics: &Metrics, rects: Vec<RenderRect>) {
        if rects.is_empty() {
            return;
        }

        // 准备矩形渲染状态.
        unsafe {
            // 从 viewport 中移除 padding.
            gl::Viewport(0, 0, size_info.width() as i32, size_info.height() as i32);
            gl::BlendFuncSeparate(gl::SRC_ALPHA, gl::ONE_MINUS_SRC_ALPHA, gl::SRC_ALPHA, gl::ONE);
        }

        self.rect_renderer.draw(size_info, metrics, rects);

        // 恢复常规状态.
        unsafe {
            // 重置混合策略.
            gl::BlendFunc(gl::SRC1_COLOR, gl::ONE_MINUS_SRC1_COLOR);

            // 恢复带 padding 的 viewport.
            self.set_viewport(size_info);
        }
    }

    /// 使用 `color` 和 `alpha` 填充窗口.
    pub fn clear(&self, color: Rgb, alpha: f32) {
        unsafe {
            gl::ClearColor(
                (f32::from(color.r) / 255.0).min(1.0) * alpha,
                (f32::from(color.g) / 255.0).min(1.0) * alpha,
                (f32::from(color.b) / 255.0).min(1.0) * alpha,
                alpha,
            );
            gl::Clear(gl::COLOR_BUFFER_BIT);
        }
    }

    /// 获取上下文重置状态.
    pub fn was_context_reset(&self) -> bool {
        // 如果不支持 robustness, 不使用其函数.
        if !self.robustness {
            return false;
        }

        let status = unsafe { gl::GetGraphicsResetStatus() };
        if status == gl::NO_ERROR {
            false
        } else {
            let reason = match status {
                gl::GUILTY_CONTEXT_RESET_KHR => "guilty",
                gl::INNOCENT_CONTEXT_RESET_KHR => "innocent",
                gl::UNKNOWN_CONTEXT_RESET_KHR => "unknown",
                _ => "invalid",
            };

            info!("GPU reset ({reason})");

            true
        }
    }

    fn supports_robustness() -> bool {
        let mut notification_strategy = 0;
        if GlExtensions::contains("GL_KHR_robustness") {
            unsafe {
                gl::GetIntegerv(gl::RESET_NOTIFICATION_STRATEGY_KHR, &mut notification_strategy);
            }
        } else {
            notification_strategy = gl::NO_RESET_NOTIFICATION_KHR as gl::types::GLint;
        }

        if notification_strategy == gl::LOSE_CONTEXT_ON_RESET_KHR as gl::types::GLint {
            info!("GPU reset notifications are enabled");
            true
        } else {
            info!("GPU reset notifications are disabled");
            false
        }
    }

    pub fn finish(&self) {
        unsafe {
            gl::Finish();
        }
    }

    /// 设置单元格渲染的 viewport.
    #[inline]
    pub fn set_viewport(&self, size: &SizeInfo) {
        unsafe {
            gl::Viewport(
                size.padding_x() as i32,
                size.padding_y() as i32,
                size.width() as i32 - 2 * size.padding_x() as i32,
                size.height() as i32 - 2 * size.padding_y() as i32,
            );
        }
    }

    /// 调整渲染器大小.
    pub fn resize(&self, size_info: &SizeInfo) {
        self.set_viewport(size_info);
        match &self.text_renderer {
            TextRendererProvider::Gles2(renderer) => renderer.resize(size_info),
            TextRendererProvider::Glsl3(renderer) => renderer.resize(size_info),
        }
    }
}

#[cfg(not(windows))]
struct GlExtensions;

#[cfg(not(windows))]
impl GlExtensions {
    /// 检查给定的 `extension` 是否受支持.
    ///
    /// 此函数将延迟加载 OpenGL 扩展.
    fn contains(extension: &str) -> bool {
        static OPENGL_EXTENSIONS: OnceLock<HashSet<&'static str, RandomState>> = OnceLock::new();

        OPENGL_EXTENSIONS.get_or_init(Self::load_extensions).contains(extension)
    }

    /// 加载可用的 OpenGL 扩展.
    fn load_extensions() -> HashSet<&'static str, RandomState> {
        unsafe {
            let extensions = gl::GetString(gl::EXTENSIONS);

            if extensions.is_null() {
                let mut extensions_number = 0;
                gl::GetIntegerv(gl::NUM_EXTENSIONS, &mut extensions_number);

                (0..extensions_number as gl::types::GLuint)
                    .flat_map(|i| {
                        let extension = CStr::from_ptr(gl::GetStringi(gl::EXTENSIONS, i) as *mut _);
                        extension.to_str()
                    })
                    .collect()
            } else {
                match CStr::from_ptr(extensions as *mut _).to_str() {
                    Ok(ext) => ext.split_whitespace().collect(),
                    Err(_) => Default::default(),
                }
            }
        }
    }
}

#[cfg(not(windows))]
extern "system" fn gl_debug_log(
    _: gl::types::GLenum,
    _: gl::types::GLenum,
    _: gl::types::GLuint,
    _: gl::types::GLenum,
    _: gl::types::GLsizei,
    msg: *const gl::types::GLchar,
    _: *mut std::os::raw::c_void,
) {
    let msg = unsafe { CStr::from_ptr(msg).to_string_lossy() };
    debug!("[gl_render] {msg}");
}
