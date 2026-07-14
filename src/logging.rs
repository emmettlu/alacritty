//! Logging for Alacritty.
//!
//! Nanologger owns the global `log` facade. Alacritty-specific behavior is
//! implemented through writer outputs for the on-demand log file and message bar.

use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, LineWriter, Write};
use std::path::PathBuf;
use std::process;
use std::sync::OnceLock;

use log::LevelFilter;
use nanologger::{LogLevel, LogOutput, LoggerBuilder};
use winit::event_loop::EventLoopProxy;

use crate::cli::Options;
use crate::event::{Event, EventType};
use crate::message_bar::{Message, MessageType};

/// Logging target for IPC config error messages.
pub const LOG_TARGET_IPC_CONFIG: &str = "alacritty_log_window_config";

/// Name for the environment variable containing the log file's path.
const ALACRITTY_LOG_ENV: &str = "ALACRITTY_LOG";

/// Logging target for config error messages.
pub const LOG_TARGET_CONFIG: &str = "alacritty_config_derive";

/// Logging target for winit events.
pub const LOG_TARGET_WINIT: &str = "alacritty_winit_event";

/// Name for the environment variable containing extra logging targets.
const ALACRITTY_EXTRA_LOG_TARGETS_ENV: &str = "ALACRITTY_EXTRA_LOG_TARGETS";

/// List of targets which will be logged by Alacritty.
const ALLOWED_TARGETS: &[&str] = &["alacritty", "crossfont"];

/// User configurable extra log targets to include.
fn extra_log_targets() -> &'static [String] {
    static EXTRA_LOG_TARGETS: OnceLock<Vec<String>> = OnceLock::new();

    EXTRA_LOG_TARGETS.get_or_init(|| {
        env::var(ALACRITTY_EXTRA_LOG_TARGETS_ENV).map_or(Vec::new(), |targets| {
            targets
                .split(';')
                .filter(|target| !target.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
    })
}

/// Initialize nanologger and install it as the global `log` facade backend.
pub fn initialize(
    options: &Options,
    event_proxy: EventLoopProxy<Event>,
) -> Result<Option<PathBuf>, nanologger::InitError> {
    let logfile = OnDemandLogFile::new();
    let path = logfile.path.clone();
    let message_bar = MessageBarWriter::new(event_proxy, path.clone());
    let module_allow = ALLOWED_TARGETS
        .iter()
        .map(|target| (*target).to_owned())
        .chain(extra_log_targets().iter().cloned())
        .collect();

    LoggerBuilder::new()
        .level(nano_level(options.log_level()).unwrap_or(LogLevel::Error))
        .module_allow(module_allow)
        // Per-output filters stay open; the global runtime level is authoritative.
        .add_output(LogOutput::term(LogLevel::Trace))
        .add_output(LogOutput::writer(LogLevel::Trace, logfile))
        .add_output(LogOutput::writer(LogLevel::Warn, message_bar))
        .init()?;

    // Nanologger has no `Off` variant, while the log facade does.
    log::set_max_level(options.log_level());

    Ok(Some(path))
}

/// Update nanologger after configuration has been loaded.
pub fn set_level(level: LevelFilter) {
    match nano_level(level) {
        Some(level) => nanologger::set_level(level),
        None => log::set_max_level(LevelFilter::Off),
    }
}

fn nano_level(level: LevelFilter) -> Option<LogLevel> {
    match level {
        LevelFilter::Off => None,
        LevelFilter::Error => Some(LogLevel::Error),
        LevelFilter::Warn => Some(LogLevel::Warn),
        LevelFilter::Info => Some(LogLevel::Info),
        LevelFilter::Debug => Some(LogLevel::Debug),
        LevelFilter::Trace => Some(LogLevel::Trace),
    }
}

struct MessageBarWriter {
    event_proxy: EventLoopProxy<Event>,
    logfile_path: PathBuf,
}

impl MessageBarWriter {
    fn new(event_proxy: EventLoopProxy<Event>, logfile_path: PathBuf) -> Self {
        Self {
            event_proxy,
            logfile_path,
        }
    }
}

impl Write for MessageBarWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let text = String::from_utf8_lossy(buf);
        let (prefix, message_type) = if text.starts_with("[ERROR]") {
            ("[ERROR]", MessageType::Error)
        } else if text.starts_with("[WARN]") {
            ("[WARN]", MessageType::Warning)
        } else {
            return Ok(buf.len());
        };
        let text = text
            .strip_prefix(prefix)
            .map(str::trim)
            .unwrap_or(text.as_ref());

        #[cfg(not(windows))]
        let env_var = format!("${ALACRITTY_LOG_ENV}");
        #[cfg(windows)]
        let env_var = format!("%{ALACRITTY_LOG_ENV}%");

        let message = Message::new(
            format!(
                "{prefix} {text}\nSee log at {} ({env_var})",
                self.logfile_path.display()
            ),
            message_type,
        );
        let _ = self
            .event_proxy
            .send_event(Event::new(EventType::Message(message), None));

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct OnDemandLogFile {
    file: Option<LineWriter<File>>,
    path: PathBuf,
}

impl OnDemandLogFile {
    fn new() -> Self {
        let mut path = env::temp_dir();
        path.push(format!("Alacritty-{}.log", process::id()));

        // Set log path as an environment variable.
        unsafe { env::set_var(ALACRITTY_LOG_ENV, path.as_os_str()) };

        Self { path, file: None }
    }

    fn file(&mut self) -> io::Result<&mut LineWriter<File>> {
        // Allow recreation if the file is deleted at runtime.
        if self.file.is_some() && !self.path.exists() {
            self.file = None;
        }

        if self.file.is_none() {
            match OpenOptions::new()
                .append(true)
                .create_new(true)
                .open(&self.path)
            {
                Ok(file) => {
                    self.file = Some(LineWriter::new(file));
                    let _ = writeln!(
                        io::stdout(),
                        "Created log file at \"{}\"",
                        self.path.display()
                    );
                }
                Err(err) => {
                    let _ = writeln!(io::stdout(), "Unable to create log file: {err}");
                    return Err(err);
                }
            }
        }

        Ok(self.file.as_mut().expect("log file initialized"))
    }
}

impl Write for OnDemandLogFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file()?.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file()?.flush()
    }
}
