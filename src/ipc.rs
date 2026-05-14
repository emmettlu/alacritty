use std::io::{Error as IoError, Result as IoResult, Write};
use std::os::unix::net::UnixStream;

use log::error;

// Re-export IPC types
pub use crate::cli::SocketReply;

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
// fn socket_dir() -> PathBuf {
//     // Try to use XDG_RUNTIME_DIR first, then fall back to temp dir
//     if let Ok(runtime_dir) = env::var("XDG_RUNTIME_DIR") {
//         let path = PathBuf::from(runtime_dir).join("alacritty");
//         let _ = fs::create_dir_all(&path);
//         return path;
//     }

//     // Fall back to temp directory
//     env::temp_dir().join("alacritty")
// }

/// Directory for the IPC socket file.
#[cfg(target_os = "macos")]
fn socket_dir() -> PathBuf {
    env::temp_dir()
}
