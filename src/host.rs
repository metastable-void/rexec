use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use chrono::Utc;
use nix::errno::Errno;
use nix::libc;
use nix::sys::signal::{self, Signal};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::Pid;

use crate::client;
use crate::filter::OutputFilter;
use crate::protocol::{
    ClientAction, ControlResponse, ERROR_ABORTED, ERROR_NOT_FOUND, ERROR_TIMEOUT, HostEvent,
    Request, Response, TranscriptEntry,
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
    silent: bool,
}

struct TranscriptQueue {
    writer: TranscriptWriter,
    state: Mutex<TranscriptQueueState>,
}

struct TranscriptQueueState {
    next: u64,
    pending: BTreeMap<u64, TranscriptRecord>,
    subscribers: Vec<TranscriptSubscriber>,
}

#[derive(Clone)]
struct TranscriptRecord {
    entry: TranscriptEntry,
    raw_output: String,
}

struct TranscriptSubscriber {
    ansi: bool,
    sender: Sender<TranscriptEntry>,
}

impl TranscriptQueue {
    fn new(writer: TranscriptWriter) -> Self {
        Self {
            writer,
            state: Mutex::new(TranscriptQueueState {
                next: 0,
                pending: BTreeMap::new(),
                subscribers: Vec::new(),
            }),
        }
    }

    fn submit(&self, seq: u64, entry: TranscriptEntry, raw_output: String) {
        let mut state = self.state.lock().unwrap();
        state
            .pending
            .insert(seq, TranscriptRecord { entry, raw_output });
        loop {
            let next = state.next;
            let Some(record) = state.pending.remove(&next) else {
                break;
            };
            let _ = self.writer.append(&record.entry);
            state.subscribers.retain(|subscriber| {
                let mut entry = record.entry.clone();
                if subscriber.ansi {
                    entry.output.clone_from(&record.raw_output);
                }
                subscriber.sender.send(entry).is_ok()
            });
            state.next += 1;
        }
    }

    fn subscribe(&self, ansi: bool) -> Receiver<TranscriptEntry> {
        let mut state = self.state.lock().unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        state
            .subscribers
            .push(TranscriptSubscriber { ansi, sender: send });
        receive
    }

    fn close_subscriptions(&self) {
        self.state.lock().unwrap().subscribers.clear();
    }
}

pub fn run() -> std::io::Result<()> {
    run_with_options(false)
}

pub fn run_with_options(silent: bool) -> std::io::Result<()> {
    SHUTDOWN.store(false, Ordering::SeqCst);
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

    if !silent {
        eprintln!("rexec host listening on {}", path.display());
        eprintln!("rexec transcript: ~/.rexec/{session_name}.jsonl");
    }

    let state = Arc::new(HostState {
        next_seq: AtomicU64::new(0),
        print_queue: Arc::new(PrintQueue::new()),
        transcript: TranscriptQueue::new(transcript),
        silent,
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

    state.transcript.close_subscriptions();
    drop(listener);
    let _ = std::fs::remove_file(&path);
    if !silent {
        eprintln!("rexec host: shutdown");
    }
    Ok(())
}

fn bind_with_stale_takeover(path: &std::path::Path) -> std::io::Result<UnixListener> {
    match UnixListener::bind(path) {
        Ok(l) => Ok(l),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => match UnixStream::connect(path) {
            Ok(stream) => {
                let verified = client::HostConnection::from_stream_with_timeout(
                    stream,
                    Duration::from_secs(2),
                )
                .is_ok();
                let message = if verified {
                    "another rexec host is already running"
                } else {
                    "another listener owns the rexec socket but did not confirm pong"
                };
                Err(std::io::Error::new(std::io::ErrorKind::AddrInUse, message))
            }
            Err(_) => {
                std::fs::remove_file(path)?;
                UnixListener::bind(path)
            }
        },
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
    raw_total: Vec<u8>,
    filtered_total: Vec<u8>,
    eof: bool,
    console_output: bool,
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

    // Every client connection starts with a ping/pong handshake. Unlike the
    // old one-shot ping, the host keeps the verified socket open for commands.
    match serde_json::from_str::<ClientAction>(line.trim_end()) {
        Ok(ClientAction::Ping) => {
            if write_control_response(&write_stream, &ControlResponse::Pong).is_err() {
                return;
            }
        }
        _ => {
            eprintln!("rexec host: connection did not start with ping");
            return;
        }
    }

    let mut command_pinged = false;
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return,
            Ok(_) => {}
            Err(err) => {
                eprintln!("rexec host: read request: {err}");
                return;
            }
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }

        if let Ok(action) = serde_json::from_str::<ClientAction>(trimmed) {
            match action {
                ClientAction::Ping => {
                    if write_control_response(&write_stream, &ControlResponse::Pong).is_err() {
                        return;
                    }
                    command_pinged = true;
                }
                ClientAction::Abort => {
                    // No command is active, so there is nothing to abort.
                }
                ClientAction::Attach { ansi } => {
                    serve_attachment(write_stream, &host, ansi);
                    return;
                }
            }
            continue;
        }

        if !command_pinged {
            eprintln!("rexec host: command request was not preceded by ping");
            return;
        }
        command_pinged = false;

        let request: Request = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(err) => {
                eprintln!("rexec host: malformed request: {err}");
                let resp = Response {
                    exit: 127,
                    output: String::new(),
                    error: Some(format!("malformed request: {err}")),
                };
                if write_response(&write_stream, &resp).is_err() {
                    return;
                }
                continue;
            }
        };

        if request.exec.is_empty() {
            let resp = Response {
                exit: 127,
                output: String::new(),
                error: Some("exec is empty".into()),
            };
            if write_response(&write_stream, &resp).is_err() {
                return;
            }
            continue;
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
                    host.silent,
                );

                let msg = format!("rexec: failed to spawn command: {err}\n");
                let resp = Response {
                    exit: 127,
                    output: msg.clone(),
                    error: Some("spawn_failed".into()),
                };
                if write_response(&write_stream, &resp).is_err() {
                    return;
                }
                host.transcript.submit(
                    seq,
                    TranscriptEntry {
                        whoami: request.whoami,
                        dir: request.dir,
                        envs: request.envs,
                        exec: request.exec,
                        timeout: request.timeout,
                        exit: 127,
                        output: msg.clone(),
                        error: Some("spawn_failed".into()),
                        time: Some(request_time),
                    },
                    msg,
                );
                continue;
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
                raw_total: Vec::new(),
                filtered_total: Vec::new(),
                eof: false,
                console_output: !host.silent,
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

        // Watch for an explicit `{"action":"abort"}` line (or client EOF). A
        // short socket timeout lets the watcher return the buffered reader after
        // the command, so this connection can parse the next request.
        let command_finished = Arc::new(AtomicBool::new(false));
        let watcher_finished = command_finished.clone();
        let _ = reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(100)));
        let abort_thread =
            std::thread::spawn(move || abort_watcher(reader, child, watcher_finished));

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
            host.silent,
        );

        let _ = reader_handle.join();
        command_finished.store(true, Ordering::SeqCst);

        let timed_out = if let Some((cancel, handle)) = timeout_thread {
            let _ = cancel.send(());
            handle.join().unwrap_or(false)
        } else {
            false
        };

        // Join the abort watcher BEFORE waitpid so it cannot race with PID reuse,
        // then restore blocking reads for the next request on this connection.
        let (returned_reader, aborted) = match abort_thread.join() {
            Ok(result) => result,
            Err(_) => return,
        };
        reader = returned_reader;
        let _ = reader.get_ref().set_read_timeout(None);

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
        let raw_output = {
            let (lock, _) = &*buf;
            let g = lock.lock().unwrap();
            String::from_utf8_lossy(&g.raw_total).into_owned()
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
        host.transcript.submit(seq, entry, raw_output);
    }
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
    silent: bool,
) -> std::thread::JoinHandle<()> {
    queue_in_print_order(print_queue, seq, move || {
        if silent {
            return;
        }
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
fn abort_watcher(
    mut reader: BufReader<UnixStream>,
    child: Pid,
    command_finished: Arc<AtomicBool>,
) -> (BufReader<UnixStream>, bool) {
    let mut line = String::new();
    loop {
        if command_finished.load(Ordering::SeqCst) {
            return (reader, false);
        }
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                kill_child_group(child);
                return (reader, true);
            }
            Ok(_) => {
                let trimmed = line.trim_end();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<ClientAction>(trimmed) {
                    Ok(ClientAction::Abort) => {
                        kill_child_group(child);
                        return (reader, true);
                    }
                    // A stray ping mid-run is meaningless; ignore it rather
                    // than treat it as a connection break.
                    Ok(ClientAction::Ping | ClientAction::Attach { .. }) => continue,
                    Err(_) => continue,
                }
            }
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => {
                kill_child_group(child);
                return (reader, true);
            }
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
                if g.console_output {
                    g.raw_pending.extend_from_slice(chunk);
                }
                g.raw_total.extend_from_slice(chunk);
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
    s.push_str(" TO=");
    s.push_str(&req.timeout.to_string());
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
        c.is_ascii_alphanumeric()
            || matches!(c, '-' | '_' | '/' | '.' | '+' | ':' | '@' | '=' | ',' | '%')
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
    Ok(())
}

fn write_control_response(stream: &UnixStream, response: &ControlResponse) -> std::io::Result<()> {
    let body = serde_json::to_string(response)
        .map_err(|e| std::io::Error::other(format!("serialize control response: {e}")))?;
    let mut s = stream;
    s.write_all(body.as_bytes())?;
    s.write_all(b"\n")?;
    s.flush()?;
    Ok(())
}

fn serve_attachment(mut stream: UnixStream, host: &HostState, ansi: bool) {
    let receive = host.transcript.subscribe(ansi);
    loop {
        match receive.recv_timeout(Duration::from_millis(250)) {
            Ok(entry) => {
                if write_host_event(&mut stream, entry).is_err() {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if attachment_disconnected(&stream) {
                    return;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn write_host_event(stream: &mut UnixStream, entry: TranscriptEntry) -> std::io::Result<()> {
    let event = HostEvent::Transcript { entry };
    let body = serde_json::to_string(&event)
        .map_err(|err| std::io::Error::other(format!("serialize host event: {err}")))?;
    stream.write_all(body.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

fn attachment_disconnected(stream: &UnixStream) -> bool {
    let mut byte = 0u8;
    let result = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            (&mut byte as *mut u8).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    if result == 0 {
        return true;
    }
    if result > 0 {
        return false;
    }
    !matches!(Errno::last(), Errno::EAGAIN | Errno::EINTR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    static NEXT_TRANSCRIPT: AtomicU64 = AtomicU64::new(0);

    fn test_host_state() -> (Arc<HostState>, std::path::PathBuf) {
        let sequence = NEXT_TRANSCRIPT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rexec-host-test-{}-{sequence}.jsonl",
            std::process::id()
        ));
        let transcript = TranscriptWriter::create_at(&path).unwrap();
        (
            Arc::new(HostState {
                next_seq: AtomicU64::new(0),
                print_queue: Arc::new(PrintQueue::new()),
                transcript: TranscriptQueue::new(transcript),
                silent: true,
            }),
            path,
        )
    }

    fn test_request(command: &str) -> Request {
        Request {
            whoami: "test".into(),
            dir: "/tmp".into(),
            envs: BTreeMap::new(),
            exec: vec![command.into()],
            stdin: None,
            timeout: 0,
        }
    }

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

    #[test]
    fn attachment_disconnect_is_detected_without_an_event() {
        let (host_end, client_end) = UnixStream::pair().unwrap();
        assert!(!attachment_disconnected(&host_end));
        drop(client_end);
        assert!(attachment_disconnected(&host_end));
    }

    #[test]
    fn verified_connection_accepts_multiple_commands() {
        let (host_end, client_end) = UnixStream::pair().unwrap();
        let (state, transcript_path) = test_host_state();
        let host = std::thread::spawn(move || handle_connection(host_end, state));
        let mut connection =
            client::HostConnection::from_stream_with_timeout(client_end, Duration::from_secs(1))
                .unwrap();

        let first = connection.execute(&test_request("true")).unwrap();
        let second = connection.execute(&test_request("true")).unwrap();
        assert_eq!(first.exit, 0);
        assert_eq!(second.exit, 0);

        drop(connection);
        host.join().unwrap();
        let entries = crate::transcript::read_entries(&transcript_path).unwrap();
        assert_eq!(entries.len(), 2);
        let _ = std::fs::remove_file(transcript_path);
    }

    #[test]
    fn command_without_fresh_ping_is_rejected() {
        let (host_end, client_end) = UnixStream::pair().unwrap();
        let (state, transcript_path) = test_host_state();
        let host = std::thread::spawn(move || handle_connection(host_end, state));
        let mut connection =
            client::HostConnection::from_stream_with_timeout(client_end, Duration::from_secs(1))
                .unwrap();

        assert!(
            connection
                .execute_after_ping(&test_request("true"))
                .is_err()
        );

        drop(connection);
        host.join().unwrap();
        let entries = crate::transcript::read_entries(&transcript_path).unwrap();
        assert!(entries.is_empty());
        let _ = std::fs::remove_file(transcript_path);
    }
}
