use log::error;
use polling::{Event, PollMode, Poller};
use rustix::fs::{CWD, Mode, OFlags, fcntl_getfl, fcntl_setfl, openat};
use rustix::process::setsid;
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::stdio::{dup2_stderr, dup2_stdin, dup2_stdout};
use rustix::termios::{InputModes, OptionalActions, Winsize, tcgetattr, tcsetattr, tcsetwinsize};
use signal_hook::low_level::{pipe as signal_pipe, unregister as unregister_signal};
use signal_hook::{SigId, consts as sigconsts};
use std::env;
use std::fs::File;
use std::io::{Error, ErrorKind, Read, Result};
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::raw::c_int;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
#[cfg(target_os = "macos")]
use std::path::Path;
use std::process::{Child, Command};
use std::sync::Arc;

use crate::terminal::event::{OnResize, WindowSize};
use crate::terminal::tty::{ChildEvent, EventedPty, EventedReadWrite, Options};

// Interest in PTY read/writes.
pub(crate) const PTY_READ_WRITE_TOKEN: usize = 0;

// Interest in new child events.
pub(crate) const PTY_CHILD_EVENT_TOKEN: usize = 1;

macro_rules! die {
    ($($arg:tt)*) => {{
        error!($($arg)*);
        std::process::exit(1);
    }};
}

// TTY controlling terminal setup is handled by opening the slave device
// in the child process after creating a new session.

#[derive(Debug)]
struct Passwd {
    name: String,
    dir: String,
    shell: String,
}

/// Return a Passwd struct with pointers into the provided buf.
///
/// # Unsafety
///
/// If `buf` is changed while `Passwd` is alive, bad thing will almost certainly happen.
fn get_pw_entry() -> Result<Passwd> {
    let uid = rustix::process::getuid().as_raw();
    let passwd =
        std::fs::read_to_string("/etc/passwd").map_err(|err| Error::other(err))?;

    for line in passwd.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let mut fields = line.split(':');
        let name = fields.next().unwrap_or("");
        let _passwd = fields.next().unwrap_or("");
        let uid_field = fields.next().unwrap_or("");
        let _gid = fields.next().unwrap_or("");
        let _gecos = fields.next().unwrap_or("");
        let dir = fields.next().unwrap_or("");
        let shell = fields.next().unwrap_or("");

        if uid_field.parse::<u32>().ok() == Some(uid) {
            return Ok(Passwd {
                name: name.to_owned(),
                dir: dir.to_owned(),
                shell: shell.to_owned(),
            });
        }
    }

    Err(Error::other("pw not found"))
}

pub struct Pty {
    child: Child,
    file: File,
    signals: UnixStream,
    sig_id: SigId,
}

/// User information that is required for a new shell session.
struct ShellUser {
    user: String,
    home: String,
    shell: String,
}

impl ShellUser {
    /// look for shell, username, longname, and home dir in the respective environment variables
    /// before falling back on looking into `passwd`.
    fn from_env() -> Result<Self> {
        let pw = get_pw_entry();

        let user = match env::var("USER") {
            Ok(user) => user,
            Err(_) => match pw {
                Ok(ref pw) => pw.name.clone(),
                Err(err) => return Err(err),
            },
        };

        let home = match env::var("HOME") {
            Ok(home) => home,
            Err(_) => match pw {
                Ok(ref pw) => pw.dir.clone(),
                Err(err) => return Err(err),
            },
        };

        let shell = match env::var("SHELL") {
            Ok(shell) => shell,
            Err(_) => match pw {
                Ok(ref pw) => pw.shell.clone(),
                Err(err) => return Err(err),
            },
        };

        Ok(Self { user, home, shell })
    }
}

#[cfg(not(target_os = "macos"))]
fn default_shell_command(shell: &str, _user: &str, _home: &str) -> Command {
    Command::new(shell)
}

#[cfg(target_os = "macos")]
fn default_shell_command(shell: &str, user: &str, home: &str) -> Command {
    let shell_name = shell.rsplit('/').next().unwrap();

    // On macOS, use the `login` command so the shell will appear as a tty session.
    let mut login_command = Command::new("/usr/bin/login");

    // Exec the shell with argv[0] prepended by '-' so it becomes a login shell.
    // `login` normally does this itself, but `-l` disables this.
    let exec = format!("exec -a -{} {}", shell_name, shell);

    // Since we use -l, `login` will not change directory to the user's home. However,
    // `login` only checks the current working directory for a .hushlogin file, causing
    // it to miss any in the user's home directory. We can fix this by doing the check
    // ourselves and passing `-q`
    let has_home_hushlogin = Path::new(home).join(".hushlogin").exists();

    // -f: Bypasses authentication for the already-logged-in user.
    // -l: Skips changing directory to $HOME and prepending '-' to argv[0].
    // -p: Preserves the environment.
    // -q: Act as if `.hushlogin` exists.
    //
    // XXX: we use zsh here over sh due to `exec -a`.
    let flags = if has_home_hushlogin { "-qflp" } else { "-flp" };
    login_command.args([flags, user, "/bin/zsh", "-fc", &exec]);
    login_command
}

/// Create a new TTY and return a handle to interact with it.
pub fn new(config: &Options, _window_size: WindowSize, window_id: u64) -> Result<Pty> {
    let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC;

    let master = openpt(flags).map_err(|err| Error::other(err.to_string()))?;
    grantpt(&master).map_err(|err| Error::other(err.to_string()))?;
    unlockpt(&master).map_err(|err| Error::other(err.to_string()))?;

    let slave_name = ptsname(&master, Vec::new()).map_err(|err| Error::other(err.to_string()))?;
    let slave = openat(
        CWD,
        &slave_name,
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|err| Error::other(err.to_string()))?;

    from_fd(config, window_id, master, slave)
}

/// Create a new TTY from a PTY's file descriptors.
pub fn from_fd(config: &Options, window_id: u64, master: OwnedFd, slave: OwnedFd) -> Result<Pty> {
    let master_fd = master.as_raw_fd();
    let slave_fd = slave.as_raw_fd();

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if let Ok(mut tios) = tcgetattr(&master) {
        #[cfg(target_os = "linux")]
        {
            // Set character encoding to UTF-8
            tios.input_modes |= InputModes::IUTF8;
        }
        let _ = tcsetattr(&master, OptionalActions::Now, &tios);
    }

    let user = ShellUser::from_env()?;

    let mut builder = if let Some(shell) = config.shell.as_ref() {
        let mut cmd = Command::new(&shell.program);
        cmd.args(shell.args.as_slice());
        cmd
    } else {
        default_shell_command(&user.shell, &user.user, &user.home)
    };

    // Setup child stdin/stdout/stderr as slave fd of PTY.
    builder.stdin(slave.try_clone()?);
    builder.stderr(slave.try_clone()?);
    builder.stdout(slave);

    // Setup shell environment.
    let window_id = window_id.to_string();
    builder.env("ALACRITTY_WINDOW_ID", &window_id);
    builder.env("USER", user.user);
    builder.env("HOME", user.home);
    // Set Window ID for clients relying on X11 hacks.
    builder.env("WINDOWID", window_id);
    for (key, value) in &config.env {
        builder.env(key, value);
    }

    // Prevent child processes from inheriting linux-specific startup notification env.
    builder.env_remove("XDG_ACTIVATION_TOKEN");
    builder.env_remove("DESKTOP_STARTUP_ID");

    let working_directory = config.working_directory.as_ref().cloned();

    let slave_name = ptsname(&master, Vec::new()).map_err(|err| Error::other(err.to_string()))?;

    unsafe {
        builder.pre_exec(move || {
            // Create a new process group.
            let _ = setsid().map_err(|err| Error::other(err.to_string()))?;

            // Set working directory, ignoring invalid paths.
            if let Some(working_directory) = working_directory.as_ref() {
                let _ = std::env::set_current_dir(working_directory);
            }

            // Reopen the slave without NOCTTY so it becomes the controlling terminal.
            let slave = openat(CWD, &slave_name, OFlags::RDWR, Mode::empty())
                .map_err(|err| Error::other(err.to_string()))?;

            dup2_stdin(&slave).map_err(|err| Error::other(err.to_string()))?;
            dup2_stdout(&slave).map_err(|err| Error::other(err.to_string()))?;
            dup2_stderr(&slave).map_err(|err| Error::other(err.to_string()))?;

            // No longer need slave/master fds.
            let _ = OwnedFd::from_raw_fd(slave_fd);
            let _ = OwnedFd::from_raw_fd(master_fd);

            Ok(())
        });
    }

    // Prepare signal handling before spawning child.
    let (signals, sig_id) = {
        let (sender, recv) = UnixStream::pair()?;

        // Register the recv end of the pipe for SIGCHLD.
        let sig_id = signal_pipe::register(sigconsts::SIGCHLD, sender)?;
        recv.set_nonblocking(true)?;
        (recv, sig_id)
    };

    match builder.spawn() {
        Ok(child) => {
            unsafe {
                // Maybe this should be done outside of this function so nonblocking
                // isn't forced upon consumers. Although maybe it should be?
                set_nonblocking(master_fd);
            }

            Ok(Pty {
                child,
                file: File::from(master),
                signals,
                sig_id,
            })
        }
        Err(err) => Err(Error::new(
            err.kind(),
            format!(
                "Failed to spawn command '{}': {}",
                builder.get_program().to_string_lossy(),
                err
            ),
        )),
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // Make sure the PTY is terminated properly.
        let _ = rustix::process::kill_process(
            rustix::process::Pid::from_child(&self.child),
            rustix::process::Signal::HUP,
        );

        // Clear signal-hook handler.
        unregister_signal(self.sig_id);

        let _ = self.child.wait();
    }
}

impl EventedReadWrite for Pty {
    type Reader = File;
    type Writer = File;

    #[inline]
    unsafe fn register(
        &mut self,
        poll: &Arc<Poller>,
        mut interest: Event,
        poll_opts: PollMode,
    ) -> Result<()> {
        interest.key = PTY_READ_WRITE_TOKEN;
        unsafe {
            poll.add_with_mode(&self.file, interest, poll_opts)?;
        }

        unsafe {
            poll.add_with_mode(
                &self.signals,
                Event::readable(PTY_CHILD_EVENT_TOKEN),
                PollMode::Level,
            )
        }
    }

    #[inline]
    fn reregister(
        &mut self,
        poll: &Arc<Poller>,
        mut interest: Event,
        poll_opts: PollMode,
    ) -> Result<()> {
        interest.key = PTY_READ_WRITE_TOKEN;
        poll.modify_with_mode(&self.file, interest, poll_opts)?;

        poll.modify_with_mode(
            &self.signals,
            Event::readable(PTY_CHILD_EVENT_TOKEN),
            PollMode::Level,
        )
    }

    #[inline]
    fn deregister(&mut self, poll: &Arc<Poller>) -> Result<()> {
        poll.delete(&self.file)?;
        poll.delete(&self.signals)
    }

    #[inline]
    fn reader(&mut self) -> &mut File {
        &mut self.file
    }

    #[inline]
    fn writer(&mut self) -> &mut File {
        &mut self.file
    }
}

impl EventedPty for Pty {
    #[inline]
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        // See if there has been a SIGCHLD.
        let mut buf = [0u8; 1];
        if let Err(err) = self.signals.read(&mut buf) {
            if err.kind() != ErrorKind::WouldBlock {
                error!("Error reading from signal pipe: {err}");
            }
            return None;
        }

        // Match on the child process.
        match self.child.try_wait() {
            Err(err) => {
                error!("Error checking child process termination: {err}");
                None
            }
            Ok(None) => None,
            Ok(exit_status) => Some(ChildEvent::Exited(exit_status)),
        }
    }
}

impl OnResize for Pty {
    /// Resize the PTY.
    ///
    /// Tells the kernel that the window size changed with the new pixel
    /// dimensions and line/column counts.
    fn on_resize(&mut self, window_size: WindowSize) {
        let win = Winsize {
            ws_row: window_size.num_lines,
            ws_col: window_size.num_cols,
            ws_xpixel: (window_size.num_cols as u32 * window_size.cell_width as u32) as u16,
            ws_ypixel: (window_size.num_lines as u32 * window_size.cell_height as u32) as u16,
        };

        if let Err(err) = tcsetwinsize(&self.file, win) {
            die!("tcsetwinsize failed: {err}");
        }
    }
}

unsafe fn set_nonblocking(fd: c_int) {
    let fd = unsafe { BorrowedFd::borrow_raw(fd) };
    let flags = fcntl_getfl(fd).unwrap_or_else(|_| OFlags::empty());
    let _ = fcntl_setfl(fd, flags | OFlags::NONBLOCK);
}

#[test]
fn test_get_pw_entry() {
    let _pw = get_pw_entry().unwrap();
}
