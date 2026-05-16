use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::thread::JoinHandle;

use log::warn;
use winit::event_loop::EventLoopProxy;

use crate::event::Event;

/// Config file update monitor.
pub struct ConfigMonitor {
    thread: JoinHandle<()>,
    #[cfg(windows)]
    shutdown_event: isize,
    #[cfg(unix)]
    shutdown_tx: std::sync::mpsc::Sender<()>,
    watched_hash: Option<u64>,
}

impl ConfigMonitor {
    pub fn new(mut paths: Vec<PathBuf>, event_proxy: EventLoopProxy<Event>) -> Option<Self> {
        // Don't monitor config if there is no path to watch.
        if paths.is_empty() {
            return None;
        }

        // Calculate the hash for the unmodified list of paths.
        let watched_hash = Self::hash_paths(&paths);

        // Exclude char devices like `/dev/null`, sockets, and so on, by checking that file type is
        // a regular file.
        paths.retain(|path| {
            // Call `metadata` to resolve symbolic links.
            path.metadata()
                .is_ok_and(|metadata| metadata.file_type().is_file())
        });

        // Canonicalize paths, keeping the base paths for symlinks.
        for i in 0..paths.len() {
            if let Ok(canonical_path) = paths[i].canonicalize() {
                match paths[i].symlink_metadata() {
                    Ok(metadata) if metadata.file_type().is_symlink() => paths.push(canonical_path),
                    _ => paths[i] = canonical_path,
                }
            }
        }

        if paths.is_empty() {
            return None;
        }

        Self::spawn(paths, watched_hash, event_proxy)
    }

    /// Synchronously shut down the monitor.
    pub fn shutdown(self) {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::Threading::SetEvent;

            let shutdown_event = self.shutdown_event as _;
            let _ = SetEvent(shutdown_event);

            if let Err(err) = self.thread.join() {
                warn!("config monitor shutdown failed: {err:?}");
            }

            CloseHandle(shutdown_event);
        }

        #[cfg(not(windows))]
        {
            let _ = self.shutdown_tx.send(());

            if let Err(err) = self.thread.join() {
                warn!("config monitor shutdown failed: {err:?}");
            }
        }
    }

    /// Check if the config monitor needs to be restarted.
    ///
    /// This checks the supplied list of files against the monitored files to determine if a
    /// restart is necessary.
    pub fn needs_restart(&self, files: &[PathBuf]) -> bool {
        Self::hash_paths(files).is_none_or(|hash| Some(hash) == self.watched_hash)
    }

    /// Generate the hash for a list of paths.
    fn hash_paths(files: &[PathBuf]) -> Option<u64> {
        // Use file count limit to avoid allocations.
        const MAX_PATHS: usize = 1024;
        if files.len() > MAX_PATHS {
            return None;
        }

        // Sort files to avoid restart on order change.
        let mut sorted_files = [None; MAX_PATHS];
        for (i, file) in files.iter().enumerate() {
            sorted_files[i] = Some(file);
        }
        sorted_files.sort_unstable();

        // Calculate hash for the paths, regardless of order.
        let mut hasher = DefaultHasher::new();
        Hash::hash_slice(&sorted_files, &mut hasher);
        Some(hasher.finish())
    }
}

#[cfg(not(windows))]
impl ConfigMonitor {
    fn spawn(
        _paths: Vec<PathBuf>,
        watched_hash: Option<u64>,
        _event_proxy: EventLoopProxy<Event>,
    ) -> Option<Self> {
        let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel();
        let thread = crate::terminal::thread::spawn_named("config watcher", move || {
            let _ = shutdown_rx.recv();
        });

        Some(Self {
            thread,
            shutdown_tx,
            watched_hash,
        })
    }
}

#[cfg(windows)]
mod windows {
    use std::collections::{HashMap, HashSet};
    use std::ffi::OsStr;
    use std::mem;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::{ptr, slice};

    use log::debug;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_IO_PENDING, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_FAILED,
        WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
        FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
        FILE_NOTIFY_CHANGE_SIZE, FILE_NOTIFY_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING, ReadDirectoryChangesW,
    };
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows_sys::Win32::System::Threading::{
        CreateEventW, INFINITE, ResetEvent, WaitForMultipleObjects,
    };
    use winit::event_loop::EventLoopProxy;

    use crate::config::monitor::ConfigMonitor;
    use crate::event::{Event, EventType};
    use crate::terminal::thread;

    const BUFFER_SIZE: usize = 16 * 1024;

    struct WatchTarget {
        primary: PathBuf,
        directories: Vec<WatchedDirectory>,
    }

    struct WatchedDirectory {
        path: PathBuf,
        file_names: HashSet<String>,
    }

    struct DirectoryWatcher {
        handle: HANDLE,
        event: HANDLE,
        overlapped: Box<OVERLAPPED>,
        buffer: Vec<u8>,
        directory: PathBuf,
    }

    impl Drop for DirectoryWatcher {
        fn drop(&mut self) {
            unsafe {
                let _ = CancelIoEx(self.handle, ptr::null());
                CloseHandle(self.event);
                CloseHandle(self.handle);
            }
        }
    }

    impl ConfigMonitor {
        pub(super) fn spawn(
            paths: Vec<PathBuf>,
            watched_hash: Option<u64>,
            event_proxy: EventLoopProxy<Event>,
        ) -> Option<Self> {
            let target = WatchTarget::new(paths)?;
            let shutdown_event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
            if shutdown_event.is_null() {
                debug!("Unable to create config watcher shutdown event");
                return None;
            }

            let shutdown_event_value = shutdown_event as isize;
            let thread = thread::spawn_named("config watcher", move || {
                let shutdown_event = shutdown_event_value as HANDLE;
                watch_loop(target, shutdown_event, event_proxy);
            });

            Some(Self {
                thread,
                shutdown_event: shutdown_event_value,
                watched_hash,
            })
        }
    }

    impl WatchTarget {
        fn new(paths: Vec<PathBuf>) -> Option<Self> {
            let primary = paths.first()?.clone();
            let mut by_directory: HashMap<PathBuf, HashSet<String>> = HashMap::new();

            for path in paths {
                let Some(parent) = path.parent().map(Path::to_path_buf) else {
                    continue;
                };
                let Some(file_name) = normalized_file_name(&path) else {
                    continue;
                };

                by_directory.entry(parent).or_default().insert(file_name);
            }

            let directories = by_directory
                .into_iter()
                .map(|(path, file_names)| WatchedDirectory { path, file_names })
                .collect::<Vec<_>>();

            (!directories.is_empty()).then_some(Self {
                primary,
                directories,
            })
        }
    }

    impl DirectoryWatcher {
        fn new(directory: &Path) -> Option<Self> {
            let path = wide_null(directory.as_os_str());
            let handle = unsafe {
                CreateFileW(
                    path.as_ptr(),
                    FILE_LIST_DIRECTORY,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                    ptr::null_mut(),
                )
            };

            if handle == INVALID_HANDLE_VALUE {
                debug!("Unable to open config directory for watching: {directory:?}");
                return None;
            }

            let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
            if event.is_null() {
                unsafe { CloseHandle(handle) };
                debug!("Unable to create config watcher event for: {directory:?}");
                return None;
            }

            let mut overlapped = Box::<OVERLAPPED>::default();
            overlapped.hEvent = event;

            let mut watcher = Self {
                handle,
                event,
                overlapped,
                buffer: vec![0; BUFFER_SIZE],
                directory: directory.to_owned(),
            };

            watcher.issue_read().then_some(watcher)
        }

        fn issue_read(&mut self) -> bool {
            unsafe {
                ResetEvent(self.event);
                *self.overlapped = mem::zeroed();
                self.overlapped.hEvent = self.event;

                let result = ReadDirectoryChangesW(
                    self.handle,
                    self.buffer.as_mut_ptr().cast(),
                    self.buffer.len() as u32,
                    0,
                    FILE_NOTIFY_CHANGE_FILE_NAME
                        | FILE_NOTIFY_CHANGE_LAST_WRITE
                        | FILE_NOTIFY_CHANGE_SIZE
                        | FILE_NOTIFY_CHANGE_CREATION,
                    ptr::null_mut(),
                    &mut *self.overlapped,
                    None,
                );

                if result != 0 || GetLastError() == ERROR_IO_PENDING {
                    true
                } else {
                    debug!("Unable to watch config directory: {:?}", self.directory);
                    false
                }
            }
        }

        fn complete(&mut self) -> Option<u32> {
            let mut bytes = 0;
            let success =
                unsafe { GetOverlappedResult(self.handle, &*self.overlapped, &mut bytes, 0) };

            (success != 0).then_some(bytes)
        }

        fn contains_watched_file(&self, bytes: u32, file_names: &HashSet<String>) -> bool {
            if bytes == 0 {
                return true;
            }

            let mut offset = 0usize;
            let bytes = bytes as usize;

            while offset + mem::size_of::<FILE_NOTIFY_INFORMATION>() <= bytes {
                let info = unsafe {
                    &*(self
                        .buffer
                        .as_ptr()
                        .add(offset)
                        .cast::<FILE_NOTIFY_INFORMATION>())
                };
                let name_len = info.FileNameLength as usize / mem::size_of::<u16>();
                let name = unsafe {
                    let slice = slice::from_raw_parts(info.FileName.as_ptr(), name_len);
                    String::from_utf16_lossy(slice).to_lowercase()
                };

                if file_names.contains(&name) {
                    return true;
                }

                if info.NextEntryOffset == 0 {
                    break;
                }

                offset += info.NextEntryOffset as usize;
            }

            false
        }
    }

    fn watch_loop(target: WatchTarget, shutdown_event: HANDLE, event_proxy: EventLoopProxy<Event>) {
        let mut watchers = target
            .directories
            .iter()
            .filter_map(|directory| DirectoryWatcher::new(&directory.path))
            .collect::<Vec<_>>();

        if watchers.is_empty() {
            debug!("No config directories could be watched");
            return;
        }

        let mut handles = Vec::with_capacity(watchers.len() + 1);
        handles.push(shutdown_event);
        handles.extend(watchers.iter().map(|watcher| watcher.event));

        loop {
            let result = unsafe {
                WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, INFINITE)
            };

            if result == WAIT_FAILED {
                debug!("Config watcher wait failed");
                return;
            }

            let index = result - WAIT_OBJECT_0;
            if index == 0 {
                return;
            }

            let watcher_index = index as usize - 1;
            let Some(watcher) = watchers.get_mut(watcher_index) else {
                continue;
            };
            let Some(directory) = target.directories.get(watcher_index) else {
                continue;
            };

            if let Some(bytes) = watcher.complete()
                && watcher.contains_watched_file(bytes, &directory.file_names)
            {
                let event = Event::new(EventType::ConfigReload(target.primary.clone()), None);
                let _ = event_proxy.send_event(event);
            }

            if !watcher.issue_read() {
                return;
            }
        }
    }

    fn normalized_file_name(path: &Path) -> Option<String> {
        Some(path.file_name()?.to_string_lossy().to_lowercase())
    }

    fn wide_null(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(Some(0)).collect()
    }
}
