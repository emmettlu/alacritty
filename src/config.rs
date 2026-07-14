use std::fmt::{self, Display, Formatter};
use std::path::{Path, PathBuf};
use std::{fs, io, process};

use log::{error, info};
use serde::Deserialize;
use toml::Value;
use toml::de::Error as TomlError;

pub mod bell;
pub mod color;
pub mod cursor;
pub mod debug;
pub mod font;
pub mod general;
pub mod scrolling;
pub mod selection;
pub mod terminal;
pub mod ui_config;
pub mod window;

mod bindings;
mod mouse;

use crate::cli::Options;
#[cfg(test)]
pub use crate::config::bindings::Binding;
pub use crate::config::bindings::{
    Action, BindingKey, BindingMode, KeyBinding, MouseAction, MouseEvent, SearchAction, ViAction,
};
pub use crate::config::ui_config::UiConfig;
use crate::logging::LOG_TARGET_CONFIG;

/// Result from config loading.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors occurring during config loading.
#[derive(Debug)]
pub enum Error {
    /// I/O error reading the configuration file.
    Io(io::Error),

    /// Invalid TOML.
    Toml(TomlError),
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => err.source(),
            Self::Toml(err) => err.source(),
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "Error reading config file: {err}"),
            Self::Toml(err) => write!(f, "Config error: {err}"),
        }
    }
}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<TomlError> for Error {
    fn from(value: TomlError) -> Self {
        Self::Toml(value)
    }
}

/// Load the configuration file.
pub fn load(options: &mut Options) -> UiConfig {
    let explicit_config = options.config_file.is_some();
    let config_path = options
        .config_file
        .clone()
        .or_else(|| installed_config("toml"));

    let mut config = match config_path.as_ref() {
        Some(config_path) => match load_from(config_path) {
            Ok(config) => config,
            Err(_) if explicit_config => process::exit(1),
            Err(_) => {
                let mut config = UiConfig::default();
                config.config_paths.push(config_path.clone());
                config
            }
        },
        None => {
            info!(target: LOG_TARGET_CONFIG, "No config file found; using default");
            UiConfig::default()
        }
    };

    options.override_config(&mut config);
    config
}

/// Load a configuration file and log errors.
fn load_from(path: &Path) -> Result<UiConfig> {
    match read_config(path) {
        Ok(config) => Ok(config),
        Err(Error::Io(io)) if io.kind() == io::ErrorKind::NotFound => {
            error!(target: LOG_TARGET_CONFIG, "Unable to load config {path:?}: File not found");
            Err(Error::Io(io))
        }
        Err(err) => {
            error!(target: LOG_TARGET_CONFIG, "Unable to load config {path:?}: {err}");
            Err(err)
        }
    }
}

/// Deserialize a single configuration file.
fn read_config(path: &Path) -> Result<UiConfig> {
    let contents = fs::read_to_string(path)?;
    let contents = contents.strip_prefix('\u{FEFF}').unwrap_or(&contents);
    let value = toml::from_str::<Value>(contents)?;
    let mut config = UiConfig::deserialize(value)?;
    config.config_paths.push(path.to_owned());
    Ok(config)
}

/// Get the default configuration file path.
pub fn installed_config(suffix: &str) -> Option<PathBuf> {
    let file_name = format!("alacritty.{suffix}");
    dirs::config_dir()
        .map(|path| path.join("alacritty").join(file_name))
        .filter(|path| path.exists())
}
