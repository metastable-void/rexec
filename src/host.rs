use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use chrono::Utc;
use nix::errno::Errno;
use nix::libc;
use nix::sys::signal::{self, Signal};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::Pid;

use crate::filter::OutputFilter;
use crate::protocol::{
    ClientAction, ControlResponse, ERROR_ABORTED, ERROR_NOT_FOUND, ERROR_TIMEOUT, Request,
    Response, TranscriptEntry,
};
use crate::pty_exec;
use crate::socket;
use crate::transcript::TranscriptWriter;

static WAKE_WRITE_FD: AtomicI32 = AtomicI32::new(-1);
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn sigint_handler(_: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
    let fd = WAKE_WRITE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = [0u8; 1];
        // async-signal-safe: write()
        unsafe {
            libc::write(fd, byte.as_ptr().cast(), 1);
        }
    }
}

struct PrintQueue {
    next: Mutex<u64>,
    cv: Condvar,
}

impl PrintQueue {
    fn new() -> Self {
        Self {
            next: Mutex::new(0),
            cv: Condvar::new(),
        }
    }
    fn wait_turn(&self, seq: u64) {
        let mut g = self.next.lock().unwrap();
        while *g < seq {
            g = self.cv.wait(g).unwrap();
        }
    }
    fn release(&self) {
        let mut g = self.next.lock().unwrap();
        *g += 1;
        self.cv.notify_all();
    }
}

struct PrintTurn<'a> {
    queue: &'a PrintQueue,
}

impl Drop for PrintTurn<'_> {
    fn drop(&mut self) {
        self.queue.release();
    }
}

struct HostState {
    next_seq: AtomicU64,
    print_queue: Arc<PrintQueue>,
    transcript: TranscriptQueue,
}

struct TranscriptQueue {
    writer: TranscriptWriter,
    state: Mutex<TranscriptQueueState>,
}

struct TranscriptQueueState {
    next: u64,
    pending: BTreeMap<u64, TranscriptEntry>,
}

impl TranscriptQueue {
    fn new(writer: TranscriptWriter) -> Self {
        Self {
            writer,
            state: Mutex::new(TranscriptQueueState {
                next: 0,
                pending: BTreeMap::new(),
            }),
        }
    }

    fn submit(&self, seq: u64, entry: TranscriptEntry) {
        let mut state = self.state.lock().unwrap();
        state.pending.insert(seq, entry);
        loop {
            let next = state.next;
            let Some(entry) = state.pending.remove(&next) else {
                break;
            };
            let _ = self.writer.append(&entry);
            state.next += 1;
        }
    }
}

pub fn run() -> std::io::Result<()> {
    let path = socket::socket_path();

    let listener = bind_with_stale_takeover(&path)?;

    // Restrict socket access to the owning user.
    let mode = libc::S_IRUSR | libc::S_IWUSR;
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| std::io::Error::other("socket path contains NUL"))?;
    unsafe {
        libc::chmod(c_path.as_ptr(), mode);
    }

    // Self-pipe + SIGINT handler so accept-blocking can be interrupted cleanly.
    let (wake_r, wake_w) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)?;
    WAKE_WRITE_FD.store(wake_w.as_raw_fd(), Ordering::Relaxed);
    install_sigint_handler()?;

    let session_name = Utc::now().format("%Y-%m-%d-%H:%M:%S").to_string();
    let transcript = TranscriptWriter::create(&session_name).map_err(|e| {
        std::io::Error::other(format!("failed to open transcript {session_name}: {e}"))
    })?;

    eprintln!("rexec host listening on {}", path.display());
    eprintln!("rexec transcript: ~/.rexec/{session_name}.jsonl");

    let state = Arc::new(HostState {
        next_seq: AtomicU64::new(0),
        print_queue: Arc::new(PrintQueue::new()),
        transcript: TranscriptQueue::new(transcript),
    });

    let listener_fd = listener.as_raw_fd();
    let wake_r_fd = wake_r.as_raw_fd();

    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }
        let mut pollfds = [
            libc::pollfd {
                fd: listener_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake_r_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let n = unsafe { libc::poll(pollfds.as_mut_ptr(), 2, -1) };
        if n < 0 {
            let err = Errno::last();
            if err == Errno::EINTR {
                continue;
            }
            eprintln!("rexec host: poll error: {err}");
            break;
        }
        if SHUTDOWN.load(Ordering::SeqCst) || pollfds[1].revents & libc::POLLIN != 0 {
            break;
        }
        if pollfds[0].revents & libc::POLLIN != 0 {
            match listener.accept() {
                Ok((stream, _)) => {
                    let state = state.clone();
                    std::thread::spawn(move || handle_connection(stream, state));
                }
                Err(err) => {
                    if SHUTDOWN.load(Ordering::SeqCst) {
                        break;
                    }
                    eprintln!("rexec host: accept error: {err}");
                }
            }
        }
    }

    drop(listener);
    let _ = std::fs::remove_file(&path);
    eprintln!("rexec host: shutdown");
    Ok(())
}

fn bind_with_stale_takeover(path: &std::path::Path) -> std::io::Result<UnixListener> {
    match UnixListener::bind(path) {
        Ok(l) => Ok(l),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            match UnixStream::connect(path) {
                Ok(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    "another rexec host is already running",
                )),
                Err(_) => {
                    std::fs::remove_file(path)?;
                    UnixListener::bind(path)
                }
            }
        }
        Err(e) => Err(e),
    }
}

fn install_sigint_handler() -> std::io::Result<()> {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = sigint_handler as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;
        if libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Ignore SIGPIPE so a closed client connection during write doesn't kill us.
        let mut sa_ign: libc::sigaction = std::mem::zeroed();
        sa_ign.sa_sigaction = libc::SIG_IGN;
        libc::sigemptyset(&mut sa_ign.sa_mask);
        libc::sigaction(libc::SIGPIPE, &sa_ign, std::ptr::null_mut());
    }
    Ok(())
}

struct PtyBuffer {
    raw_pending: Vec<u8>,
    filtered_total: Vec<u8>,
    eof: bool,
}

fn handle_connection(stream: UnixStream, host: Arc<HostState>) {
    let write_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(err) => {
            eprintln!("rexec host: try_clone failed: {err}");
            return;
        }
    };

    let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => return, // Connect-then-close probe; nothing to do.
        Ok(_) => {}
        Err(err) => {
            eprintln!("rexec host: read request: {err}");
            return;
        }
    }
    let _ = reader.get_ref().set_read_timeout(None);

    let trimmed = line.trim_end();

    // The first line is either a control action (ping / stray abort) or a
    // command Request. Try the tagged enum first; on failure fall through to
    // Request parsing.
    if let Ok(action) = serde_json::from_str::<ClientAction>(trimmed) {
        match action {
            ClientAction::Ping => {
                let _ = write_control_response(&write_stream, &ControlResponse::Pong);
            }
            ClientAction::Abort => {
                // No run in progress to abort; just close.
            }
        }
        return;
    }

    let request: Request = match serde_json::from_str(trimmed) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("rexec host: malformed request: {err}");
            let resp = Response {
                exit: 127,
                output: String::new(),
                error: Some(format!("malformed request: {err}")),
            };
            let _ = write_response(&write_stream, &resp);
            return;
        }
    };

    if request.exec.is_empty() {
        let resp = Response {
            exit: 127,
            output: String::new(),
            error: Some("exec is empty".into()),
        };
        let _ = write_response(&write_stream, &resp);
        return;
    }

    let seq = host.next_seq.fetch_add(1, Ordering::SeqCst);
    let request_time = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let envs_vec: Vec<(String, String)> = request
        .envs
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let spawn_result = pty_exec::spawn(
        &request.exec,
        &envs_vec,
        &request.dir,
        request.stdin.is_some(),
    );

    let spawned = match spawn_result {
        Ok(s) => s,
        Err(err) => {
            let _ = queue_console_output(
                host.print_queue.clone(),
                seq,
                request.clone(),
                request_time.clone(),
                None,
            );

            let msg = format!("rexec: failed to spawn command: {err}\n");
            let resp = Response {
                exit: 127,
                output: msg.clone(),
                error: Some("spawn_failed".into()),
            };
            let _ = write_response(&write_stream, &resp);
            host.transcript.submit(
                seq,
                TranscriptEntry {
                    whoami: request.whoami,
                    dir: request.dir,
                    envs: request.envs,
                    exec: request.exec,
                    timeout: request.timeout,
                    exit: 127,
                    output: msg,
                    error: Some("spawn_failed".into()),
                    time: Some(request_time),
                },
            );
            return;
        }
    };

    let pty_exec::Spawned {
        master,
        child,
        errno_pipe_read,
        stdin_write,
    } = spawned;

    // Feed the child's stdin in the background. The thread closes the pipe on
    // drop, sending EOF; an unread tail just produces EPIPE which we ignore.
    if let Some(stdin_fd) = stdin_write {
        let bytes = request.stdin.clone().unwrap_or_default().into_bytes();
        std::thread::spawn(move || {
            let mut file = std::fs::File::from(stdin_fd);
            let _ = file.write_all(&bytes);
        });
    }

    let buf = Arc::new((
        Mutex::new(PtyBuffer {
            raw_pending: Vec::new(),
            filtered_total: Vec::new(),
            eof: false,
        }),
        Condvar::new(),
    ));

    let reader_buf = buf.clone();
    let reader_handle = std::thread::spawn(move || pty_reader(master, reader_buf));

    // Start the timeout as soon as the process is running, rather than when it
    // reaches the front of the console print queue. The child is a session and
    // process-group leader, so killing its group also catches pager descendants.
    let timeout_thread = if request.timeout == 0 {
        None
    } else {
        let (cancel_tx, cancel_rx) = std::sync::mpsc::channel();
        let timeout = Duration::from_secs(request.timeout);
        let handle = std::thread::spawn(move || {
            if timeout_expired(&cancel_rx, timeout) {
                kill_child_group(child);
                true
            } else {
                false
            }
        });
        Some((cancel_tx, handle))
    };

    // Watch for an explicit `{"action":"abort"}` line (or client EOF) and kill
    // the child's process group if the client bails before we send a response.
    let abort_thread = std::thread::spawn(move || abort_watcher(reader, child));

    // Console output remains ordered by request arrival, but printing happens
    // independently of this connection worker. A command that finishes while
    // an earlier command owns the console can therefore return immediately to
    // its CLI or MCP caller.
    let _ = queue_console_output(
        host.print_queue.clone(),
        seq,
        request.clone(),
        request_time.clone(),
        Some(buf.clone()),
    );

    let _ = reader_handle.join();

    let timed_out = if let Some((cancel, handle)) = timeout_thread {
        let _ = cancel.send(());
        handle.join().unwrap_or(false)
    } else {
        false
    };

    // Wake the abort watcher (locally close the read side) and join it BEFORE
    // waitpid so it cannot race with PID reuse.
    let _ = write_stream.shutdown(std::net::Shutdown::Read);
    let aborted = abort_thread.join().unwrap_or(false);

    let exit_code = wait_for_child(child);
    let errno = pty_exec::read_errno(&errno_pipe_read).unwrap_or(None);

    let (response_error, exit_for_response) = if errno.is_some() {
        (Some(ERROR_NOT_FOUND.to_string()), 127)
    } else if timed_out {
        (Some(ERROR_TIMEOUT.to_string()), 124)
    } else if aborted {
        (Some(ERROR_ABORTED.to_string()), exit_code)
    } else {
        (None, exit_code)
    };

    let filtered_output = {
        let (lock, _) = &*buf;
        let g = lock.lock().unwrap();
        String::from_utf8_lossy(&g.filtered_total).into_owned()
    };

    let response = Response {
        exit: exit_for_response,
        output: filtered_output.clone(),
        error: response_error.clone(),
    };
    let _ = write_response(&write_stream, &response);

    let entry = TranscriptEntry {
        whoami: request.whoami,
        dir: request.dir,
        envs: request.envs,
        exec: request.exec,
        timeout: request.timeout,
        exit: exit_for_response,
        output: filtered_output,
        error: response_error,
        time: Some(request_time),
    };
    host.transcript.submit(seq, entry);
}

fn timeout_expired(cancel: &Receiver<()>, timeout: Duration) -> bool {
    matches!(cancel.recv_timeout(timeout), Err(RecvTimeoutError::Timeout))
}

fn queue_console_output(
    print_queue: Arc<PrintQueue>,
    seq: u64,
    request: Request,
    request_time: String,
    buf: Option<Arc<(Mutex<PtyBuffer>, Condvar)>>,
) -> std::thread::JoinHandle<()> {
    queue_in_print_order(print_queue, seq, move || {
        print_banner(&request, &request_time);
        if let Some(buf) = buf {
            drain_to_stdout(&buf);
        }
        print_extra_newline();
    })
}

fn queue_in_print_order<F>(
    print_queue: Arc<PrintQueue>,
    seq: u64,
    job: F,
) -> std::thread::JoinHandle<()>
where
    F: FnOnce() + Send + 'static,
{
    std::thread::spawn(move || {
        print_queue.wait_turn(seq);
        let _turn = PrintTurn {
            queue: &print_queue,
        };
        job();
    })
}

// Reads JSONL action lines from the client after the initial request. Returns
// true if an abort was honoured (or the client disconnected while the child was
// still running). Sends SIGTERM, then SIGKILL after a short grace, to the
// child's process group (forkpty makes the child a session/PG leader).
fn abort_watcher(mut reader: BufReader<UnixStream>, child: Pid) -> bool {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return false,
            Ok(_) => {
                let trimmed = line.trim_end();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<ClientAction>(trimmed) {
                    Ok(ClientAction::Abort) => {
                        kill_child_group(child);
                        return true;
                    }
                    // A stray ping mid-run is meaningless; ignore it rather
                    // than treat it as a connection break.
                    Ok(ClientAction::Ping) => continue,
                    Err(_) => continue,
                }
            }
            Err(_) => return false,
        }
    }
}

fn kill_child_group(child: Pid) {
    let _ = signal::killpg(child, Signal::SIGTERM);
    std::thread::sleep(Duration::from_millis(200));
    let _ = signal::killpg(child, Signal::SIGKILL);
}

fn pty_reader(master: OwnedFd, buf: Arc<(Mutex<PtyBuffer>, Condvar)>) {
    let mut file = std::fs::File::from(master);
    let mut filter = OutputFilter::new();
    let mut tmp = [0u8; 8192];
    let (lock, cv) = &*buf;
    loop {
        match file.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                let chunk = &tmp[..n];
                let mut g = lock.lock().unwrap();
                g.raw_pending.extend_from_slice(chunk);
                filter.push(chunk, &mut g.filtered_total);
                cv.notify_all();
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
            Err(_) => break,
        }
    }
    let mut g = lock.lock().unwrap();
    g.eof = true;
    cv.notify_all();
}

fn drain_to_stdout(buf: &Arc<(Mutex<PtyBuffer>, Condvar)>) {
    let (lock, cv) = &**buf;
    loop {
        let mut g = lock.lock().unwrap();
        while g.raw_pending.is_empty() && !g.eof {
            g = cv.wait(g).unwrap();
        }
        let chunk = std::mem::take(&mut g.raw_pending);
        let eof = g.eof;
        drop(g);
        if !chunk.is_empty() {
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            let _ = out.write_all(&chunk);
            let _ = out.flush();
        }
        if eof {
            return;
        }
    }
}

fn wait_for_child(pid: Pid) -> i32 {
    loop {
        match waitpid(pid, None) {
            Ok(WaitStatus::Exited(_, code)) => return code,
            Ok(WaitStatus::Signaled(_, sig, _)) => return 128 + sig as i32,
            Ok(_) => continue,
            Err(Errno::EINTR) => continue,
            Err(_) => return 127,
        }
    }
}

fn print_banner(req: &Request, ts: &str) {
    let mut s = String::new();
    s.push('[');
    s.push_str(ts);
    s.push_str("] ");
    s.push_str(&req.whoami);
    s.push(':');
    s.push_str(&req.dir);
    s.push_str(" $");
    for arg in &req.exec {
        s.push(' ');
        s.push_str(&shell_quote(arg));
    }
    s.push('\n');
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = out.write_all(s.as_bytes());
    let _ = out.flush();
}

fn print_extra_newline() {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

fn shell_quote(arg: &str) -> Cow<'_, str> {
    fn is_safe(c: char) -> bool {
        c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '.' | '+' | ':' | '@' | '=' | ',' | '%')
    }
    if !arg.is_empty() && arg.chars().all(is_safe) {
        return Cow::Borrowed(arg);
    }
    let mut s = String::with_capacity(arg.len() + 2);
    s.push('\'');
    for c in arg.chars() {
        if c == '\'' {
            s.push_str("'\\''");
        } else {
            s.push(c);
        }
    }
    s.push('\'');
    Cow::Owned(s)
}

fn write_response(stream: &UnixStream, response: &Response) -> std::io::Result<()> {
    let body = serde_json::to_string(response)
        .map_err(|e| std::io::Error::other(format!("serialize response: {e}")))?;
    let mut s = stream;
    s.write_all(body.as_bytes())?;
    s.write_all(b"\n")?;
    s.flush()?;
    use std::net::Shutdown;
    let _ = stream.shutdown(Shutdown::Write);
    Ok(())
}

fn write_control_response(stream: &UnixStream, response: &ControlResponse) -> std::io::Result<()> {
    let body = serde_json::to_string(response)
        .map_err(|e| std::io::Error::other(format!("serialize control response: {e}")))?;
    let mut s = stream;
    s.write_all(body.as_bytes())?;
    s.write_all(b"\n")?;
    s.flush()?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_wait_can_be_cancelled() {
        let (send, receive) = std::sync::mpsc::channel();
        send.send(()).unwrap();
        assert!(!timeout_expired(&receive, Duration::from_secs(1)));
    }

    #[test]
    fn disconnected_timeout_wait_is_not_an_expiration() {
        let (send, receive) = std::sync::mpsc::channel();
        drop(send);
        assert!(!timeout_expired(&receive, Duration::from_secs(1)));
    }

    #[test]
    fn timeout_wait_reports_expiration() {
        let (_send, receive) = std::sync::mpsc::channel();
        assert!(timeout_expired(&receive, Duration::from_millis(1)));
    }

    #[test]
    fn queuing_console_output_does_not_wait_for_its_turn() {
        let print_queue = Arc::new(PrintQueue::new());
        let (send, receive) = std::sync::mpsc::channel();

        // Sequence 1 cannot print until sequence 0 releases, but enqueueing it
        // returns a handle immediately instead of waiting on the queue.
        let handle = queue_in_print_order(print_queue.clone(), 1, move || {
            send.send(()).unwrap();
        });
        assert_eq!(*print_queue.next.lock().unwrap(), 0);
        assert!(matches!(
            receive.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));

        print_queue.release();
        receive.recv_timeout(Duration::from_secs(1)).unwrap();
        handle.join().unwrap();
        assert_eq!(*print_queue.next.lock().unwrap(), 2);
    }
}
