use std::io::{BufRead, BufReader, Error as IoError, Result as IoResult, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::{env, fs, process};

use log::error;

// Re-export IPC types for event handling
pub use crate::cli::{SocketMessage, SocketReply};

/// Environment variable name for the IPC socket path.
const ALACRITTY_SOCKET_ENV: &str = "ALACRITTY_SOCKET";

/// IPC socket listener.
pub struct IpcListener {
    socket: UnixListener,
    data: String,
}

impl IpcListener {
    /// Create a new IPC listener bound to the given path.
    pub fn new(path: &Path) -> IoResult<Self> {
        // Remove any stale socket file.
        let _ = fs::remove_file(path);

        let socket = UnixListener::bind(path)?;
        socket.set_nonblocking(true)?;

        // Expose the path to child processes (shells, fastfetch, etc).
        unsafe { env::set_var(ALACRITTY_SOCKET_ENV, path.as_os_str()) };

        Ok(Self {
            socket,
            data: String::new(),
        })
    }

    /// Try to receive one IPC message without blocking.
    /// Returns Some((message, optional_stream)) when a message was received.
    /// The stream is provided only for GetConfig requests so the caller can reply on it.
    pub fn try_recv(&mut self) -> Option<(SocketMessage, Option<UnixStream>)> {
        let (stream, _) = self.socket.accept().ok()?;

        self.data.clear();
        let mut reader = BufReader::new(&stream);
        if reader.read_line(&mut self.data).ok()? == 0 {
            return None;
        }

        let message: SocketMessage = serde_json::from_str(&self.data).ok()?;

        match &message {
            SocketMessage::GetConfig(_) => Some((message, Some(stream))),
            _ => Some((message, None)),
        }
    }
}

impl Drop for IpcListener {
    fn drop(&mut self) {
        // Best effort cleanup of the socket file.
        if let Ok(addr) = self.socket.local_addr()
            && let Some(path) = addr.as_pathname()
        {
            let _ = fs::remove_file(path);
        }
    }
}

/// Send IPC message reply.
pub fn send_reply(stream: &mut UnixStream, message: SocketReply) {
    if let Err(err) = send_reply_fallible(stream, message) {
        error!("Failed to send IPC reply: {err}");
    }
}

/// Send IPC message reply, returning possible errors.
fn send_reply_fallible(stream: &mut UnixStream, message: SocketReply) -> IoResult<()> {
    let json = serde_json::to_string(&message).map_err(IoError::other)?;
    stream.write_all(json.as_bytes())?;
    stream.flush()?;
    Ok(())
}

/// Directory for the IPC socket file.
#[cfg(not(target_os = "macos"))]
pub fn socket_dir() -> PathBuf {
    if let Ok(runtime_dir) = env::var("XDG_RUNTIME_DIR") {
        let path = PathBuf::from(runtime_dir).join("alacritty");
        if fs::create_dir_all(&path).is_ok() {
            return path;
        }
    }
    env::temp_dir().join("alacritty")
}

/// Directory for the IPC socket file.
#[cfg(target_os = "macos")]
pub fn socket_dir() -> PathBuf {
    env::temp_dir()
}

/// File prefix matching all available sockets for this display.
#[cfg(not(target_os = "macos"))]
pub fn socket_prefix() -> String {
    let display = env::var("WAYLAND_DISPLAY")
        .or_else(|_| env::var("DISPLAY"))
        .unwrap_or_default();
    format!("Alacritty-{}", display.replace('/', "-"))
}

/// File prefix matching all available sockets.
#[cfg(target_os = "macos")]
pub fn socket_prefix() -> String {
    String::from("Alacritty")
}

/// Return a unique socket path for this Alacritty instance.
pub fn socket_path() -> PathBuf {
    let dir = socket_dir();
    let _ = fs::create_dir_all(&dir);
    let prefix = socket_prefix();
    let pid = process::id();
    dir.join(format!("{}-{}.sock", prefix, pid))
}
