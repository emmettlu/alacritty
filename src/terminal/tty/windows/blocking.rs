use std::collections::VecDeque;
use std::io;
use std::io::prelude::*;
use std::marker::PhantomData;
use std::sync::{Arc, Condvar, Mutex};
use std::task::Wake;

use polling::os::iocp::{CompletionPacket, PollerIocpExt};
use polling::{Event, PollMode, Poller};

use crate::terminal::thread::spawn_named;

struct Registration {
    interest: Mutex<Option<Interest>>,
    end: PipeEnd,
}

#[derive(Copy, Clone)]
enum PipeEnd {
    Reader,
    Writer,
}

struct Interest {
    /// The event to send about completion.
    event: Event,

    /// The poller to send the event to.
    poller: Arc<Poller>,

    /// The mode that we are in.
    mode: PollMode,
}

struct PipeState {
    buffer: VecDeque<u8>,
    closed: bool,
}

struct BlockingPipe {
    state: Mutex<PipeState>,
    readable: Condvar,
    writable: Condvar,
    capacity: usize,
}

impl BlockingPipe {
    fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(PipeState {
                buffer: VecDeque::with_capacity(capacity),
                closed: false,
            }),
            readable: Condvar::new(),
            writable: Condvar::new(),
            capacity,
        }
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        self.readable.notify_all();
        self.writable.notify_all();
    }

    fn is_empty(&self) -> bool {
        self.state.lock().unwrap().buffer.is_empty()
    }

    fn is_full(&self) -> bool {
        self.state.lock().unwrap().buffer.len() >= self.capacity
    }

    fn push_blocking(&self, data: &[u8]) -> bool {
        let mut written = 0;

        while written < data.len() {
            let mut state = self.state.lock().unwrap();
            while state.buffer.len() >= self.capacity && !state.closed {
                state = self.writable.wait(state).unwrap();
            }

            if state.closed {
                return false;
            }

            let available = self.capacity - state.buffer.len();
            let end = (written + available).min(data.len());
            state.buffer.extend(&data[written..end]);
            written = end;

            self.readable.notify_all();
        }

        true
    }

    fn pop_blocking(&self, out: &mut Vec<u8>) -> bool {
        let mut state = self.state.lock().unwrap();
        while state.buffer.is_empty() && !state.closed {
            state = self.readable.wait(state).unwrap();
        }

        if state.buffer.is_empty() && state.closed {
            return false;
        }

        out.extend(state.buffer.drain(..));
        self.writable.notify_all();
        true
    }

    fn try_read(&self, buf: &mut [u8]) -> usize {
        let mut state = self.state.lock().unwrap();
        let len = buf.len().min(state.buffer.len());

        for byte in buf.iter_mut().take(len) {
            *byte = state.buffer.pop_front().unwrap();
        }

        if len > 0 {
            self.writable.notify_all();
        }

        len
    }

    fn try_write(&self, buf: &[u8]) -> usize {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return 0;
        }

        let available = self.capacity.saturating_sub(state.buffer.len());
        let len = available.min(buf.len());
        state.buffer.extend(&buf[..len]);

        if len > 0 {
            self.readable.notify_all();
        }

        len
    }
}

/// Poll a reader in another thread.
pub struct UnblockedReader<R> {
    /// The event to send about completion.
    interest: Arc<Registration>,

    /// The pipe that we are reading from.
    pipe: Arc<BlockingPipe>,

    /// Is this the first time registering?
    first_register: bool,

    /// We logically own the reader, but we don't actually use it.
    _reader: PhantomData<R>,
}

impl<R: Read + Send + 'static> UnblockedReader<R> {
    /// Spawn a new unblocked reader.
    pub fn new(mut source: R, pipe_capacity: usize) -> Self {
        let pipe = Arc::new(BlockingPipe::new(pipe_capacity));
        let interest = Arc::new(Registration {
            interest: Mutex::<Option<Interest>>::new(None),
            end: PipeEnd::Reader,
        });

        let thread_pipe = Arc::clone(&pipe);
        let thread_interest = Arc::clone(&interest);

        spawn_named("alacritty-tty-reader-thread", move || {
            let mut buf = vec![0; pipe_capacity.max(1)];

            loop {
                match source.read(&mut buf) {
                    Ok(0) => {
                        thread_pipe.close();
                        thread_interest.wake_by_ref();
                        return;
                    }
                    Ok(n) => {
                        if !thread_pipe.push_blocking(&buf[..n]) {
                            return;
                        }
                        thread_interest.wake_by_ref();
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        log::error!("error reading from pipe source: {e}");
                        thread_pipe.close();
                        thread_interest.wake_by_ref();
                        return;
                    }
                }
            }
        });

        Self {
            interest,
            pipe,
            first_register: true,
            _reader: PhantomData,
        }
    }

    /// Register interest in the reader.
    pub fn register(&mut self, poller: &Arc<Poller>, event: Event, mode: PollMode) {
        let mut interest = self.interest.interest.lock().unwrap();
        *interest = Some(Interest {
            event,
            poller: poller.clone(),
            mode,
        });

        // Send the event to start off with if we have any data.
        if (!self.pipe.is_empty() && event.readable) || self.first_register {
            self.first_register = false;
            poller.post(CompletionPacket::new(event)).ok();
        }
    }

    /// Deregister interest in the reader.
    pub fn deregister(&self) {
        let mut interest = self.interest.interest.lock().unwrap();
        *interest = None;
    }

    /// Try to read from the reader.
    pub fn try_read(&mut self, buf: &mut [u8]) -> usize {
        let len = self.pipe.try_read(buf);
        if len > 0 {
            self.interest.wake_by_ref();
        }
        len
    }
}

impl<R> Drop for UnblockedReader<R> {
    fn drop(&mut self) {
        self.pipe.close();
    }
}

impl<R: Read + Send + 'static> Read for UnblockedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        Ok(self.try_read(buf))
    }
}

/// Poll a writer in another thread.
pub struct UnblockedWriter<W> {
    /// The interest to send about completion.
    interest: Arc<Registration>,

    /// The pipe that we are writing to.
    pipe: Arc<BlockingPipe>,

    /// We logically own the writer, but we don't actually use it.
    _reader: PhantomData<W>,
}

impl<W: Write + Send + 'static> UnblockedWriter<W> {
    /// Spawn a new unblocked writer.
    pub fn new(mut sink: W, pipe_capacity: usize) -> Self {
        let pipe = Arc::new(BlockingPipe::new(pipe_capacity));
        let interest = Arc::new(Registration {
            interest: Mutex::<Option<Interest>>::new(None),
            end: PipeEnd::Writer,
        });

        let thread_pipe = Arc::clone(&pipe);
        let thread_interest = Arc::clone(&interest);

        spawn_named("alacritty-tty-writer-thread", move || {
            let mut buf = Vec::with_capacity(pipe_capacity.max(1));

            loop {
                buf.clear();
                if !thread_pipe.pop_blocking(&mut buf) {
                    return;
                }

                if let Err(e) = sink.write_all(&buf) {
                    if e.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }

                    log::error!("error writing to pipe sink: {e}");
                    thread_pipe.close();
                    thread_interest.wake_by_ref();
                    return;
                }

                thread_interest.wake_by_ref();
            }
        });

        Self {
            interest,
            pipe,
            _reader: PhantomData,
        }
    }

    /// Register interest in the writer.
    pub fn register(&self, poller: &Arc<Poller>, event: Event, mode: PollMode) {
        let mut interest = self.interest.interest.lock().unwrap();
        *interest = Some(Interest {
            event,
            poller: poller.clone(),
            mode,
        });

        // Send the event to start off with if we have room for data.
        if !self.pipe.is_full() && event.writable {
            poller.post(CompletionPacket::new(event)).ok();
        }
    }

    /// Deregister interest in the writer.
    pub fn deregister(&self) {
        let mut interest = self.interest.interest.lock().unwrap();
        *interest = None;
    }

    /// Try to write to the writer.
    pub fn try_write(&mut self, buf: &[u8]) -> usize {
        let len = self.pipe.try_write(buf);
        if len > 0 {
            self.interest.wake_by_ref();
        }
        len
    }
}

impl<W> Drop for UnblockedWriter<W> {
    fn drop(&mut self) {
        self.pipe.close();
    }
}

impl<W: Write + Send + 'static> Write for UnblockedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Ok(self.try_write(buf))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Wake for Registration {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        let mut interest_lock = self.interest.lock().unwrap();
        if let Some(interest) = interest_lock.as_ref() {
            // Send the event to the poller.
            let send_event = match self.end {
                PipeEnd::Reader => interest.event.readable,
                PipeEnd::Writer => interest.event.writable,
            };

            if send_event {
                interest
                    .poller
                    .post(CompletionPacket::new(interest.event))
                    .ok();

                // Clear the event if we're in oneshot mode.
                if matches!(interest.mode, PollMode::Oneshot | PollMode::EdgeOneshot) {
                    *interest_lock = None;
                }
            }
        }
    }
}
