//! Miscellaneous configuration options.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// General config section.
///
/// This section is for fields which can not be easily categorized,
/// to avoid common TOML issues with root-level fields.
#[derive(Deserialize, Serialize, Clone, PartialEq, Debug)]
#[cfg_attr(not(unix), derive(Default))]
#[serde(default)]
pub struct General {
    /// Configuration file imports.
    ///
    /// This is never read since the field is directly accessed through the config's
    /// [`toml::Value`], but still present to prevent unused field warnings.
    pub import: Vec<String>,

    /// Shell startup directory.
    pub working_directory: Option<PathBuf>,

    /// Offer IPC through a unix socket.
    #[cfg(unix)]
    #[serde(default = "default_true")]
    #[allow(unused)]
    pub ipc_socket: bool,
}

#[cfg(unix)]
impl Default for General {
    fn default() -> Self {
        Self {
            ipc_socket: true,
            working_directory: Default::default(),
            import: Default::default(),
        }
    }
}

#[cfg(unix)]
fn default_true() -> bool {
    true
}
