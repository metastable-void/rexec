use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use nix::libc;

use crate::cli::{ColorChoice, RunArgs};
use crate::protocol::{
    ABORT_LINE, ClientAction, ControlResponse, ERROR_NOT_FOUND, HostEvent, PING_LINE, Request,
    Response, TranscriptEntry,
};
use crate::socket;
use crate::transcript;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

/// A ping-verified host socket that can execute multiple sequential requests.
/// The MCP server pools these and reuses them until either side disconnects;
/// one-shot CLI operations use the same handshake and framing.
pub(crate) struct HostConnection {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl HostConnection {
    pub(crate) fn connect(path: &Path) -> std::io::Result<Self> {
        Self::connect_with_timeout(path, HANDSHAKE_TIMEOUT)
    }

    pub(crate) fn connect_with_timeout(path: &Path, timeout: Duration) -> std::io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        Self::from_stream_with_timeout(stream, timeout)
    }

    pub(crate) fn from_stream_with_timeout(
        stream: UnixStream,
        timeout: Duration,
    ) -> std::io::Result<Self> {
        let reader = BufReader::new(stream.try_clone()?);
        let mut connection = Self { stream, reader };
        connection.ping_with_timeout(timeout)?;
        Ok(connection)
    }

    pub(crate) fn execute(&mut self, request: &Request) -> std::io::Result<Response> {
        self.ping()?;
        self.execute_after_ping(request)
    }

    pub(crate) fn ping(&mut self) -> std::io::Result<()> {
        self.ping_with_timeout(HANDSHAKE_TIMEOUT)
    }

    pub(crate) fn execute_after_ping(&mut self, request: &Request) -> std::io::Result<Response> {
        send_request(&self.stream, request)?;
        read_response(&mut self.reader)
    }

    pub(crate) fn ping_with_timeout(&mut self, timeout: Duration) -> std::io::Result<()> {
        self.stream.set_read_timeout(Some(timeout))?;
        self.stream.set_write_timeout(Some(timeout))?;
        let result = self.exchange_ping();
        let clear_read = self.stream.set_read_timeout(None);
        let clear_write = self.stream.set_write_timeout(None);
        result?;
        clear_read?;
        clear_write
    }

    fn exchange_ping(&mut self) -> std::io::Result<()> {
        let mut writer = &self.stream;
        writer.write_all(PING_LINE)?;
        writer.flush()?;

        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "host disconnected before pong",
            ));
        }
        match serde_json::from_str::<ControlResponse>(line.trim_end()) {
            Ok(ControlResponse::Pong) => Ok(()),
            Err(err) => Err(std::io::Error::other(format!("invalid host pong: {err}"))),
        }
    }

    pub(crate) fn is_disconnected(&self) -> bool {
        let mut byte = 0u8;
        let result = unsafe {
            libc::recv(
                self.stream.as_raw_fd(),
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
        !matches!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(code)
                if code == libc::EAGAIN || code == libc::EWOULDBLOCK || code == libc::EINTR
        )
    }

    fn stream(&self) -> &UnixStream {
        &self.stream
    }

    fn reader(&mut self) -> &mut BufReader<UnixStream> {
        &mut self.reader
    }
}

/// Send a single request to the host and return the response. Installs no
/// signal handlers and does no I/O on stdout/stderr; the caller decides how to
/// render results.
pub fn exec_blocking(request: &Request) -> Result<Response, String> {
    let mut connection = HostConnection::connect(&socket::socket_path())
        .map_err(|_| "HOST NOT FOUND".to_string())?;
    connection
        .execute(request)
        .map_err(|e| format!("host: {e}"))
}

pub fn check_host() -> i32 {
    let _connection = match HostConnection::connect(&socket::socket_path()) {
        Ok(connection) => connection,
        Err(_) => {
            println!("HOST NOT FOUND");
            return 127;
        }
    };

    println!("HOST RUNNING");
    0
}

pub fn run(args: RunArgs) -> i32 {
    let mut connection = match HostConnection::connect(&socket::socket_path()) {
        Ok(connection) => connection,
        Err(_) => {
            eprintln!("HOST NOT FOUND");
            return 127;
        }
    };

    let dir_str = match args.dir.to_str() {
        Some(s) => s.to_string(),
        None => {
            eprintln!("rexec: --dir contains invalid UTF-8");
            return 127;
        }
    };

    let mut envs = BTreeMap::new();
    for (k, v) in args.envs {
        envs.insert(k, v);
    }

    let stdin = if args.read_stdin {
        match read_stdin_to_string() {
            Ok(s) => Some(s),
            Err(err) => {
                eprintln!("rexec: failed to read stdin: {err}");
                return 127;
            }
        }
    } else {
        None
    };

    let request = Request {
        whoami: args.whoami,
        dir: dir_str,
        envs,
        exec: args.argv.clone(),
        stdin,
        timeout: args.timeout,
    };

    install_abort_handlers();
    let mut abort_guard = AbortGuard::arm(connection.stream().as_raw_fd());
    let response = match connection.execute(&request) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("rexec: host request failed: {err}");
            abort_guard.disarm();
            return 127;
        }
    };

    abort_guard.disarm();

    if response.error.as_deref() == Some(ERROR_NOT_FOUND) {
        let arg0 = args.argv.first().map(String::as_str).unwrap_or("");
        eprintln!("{arg0}: not found");
        return response.exit;
    }

    if !response.output.is_empty() {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        let _ = out.write_all(response.output.as_bytes());
        let _ = out.flush();
    }

    if let Some(err) = &response.error {
        eprintln!("rexec: host reported error: {err}");
    }

    response.exit
}

// Stream fd used by the signal handler to write the abort line. -1 = not armed.
static ABORT_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn abort_signal_handler(signum: libc::c_int) {
    let fd = ABORT_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        // async-signal-safe: write() on a short buffer.
        unsafe {
            libc::write(fd, ABORT_LINE.as_ptr().cast(), ABORT_LINE.len());
        }
    }
    // Restore default disposition and re-raise so the process actually dies.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = libc::SIG_DFL;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;
        libc::sigaction(signum, &sa, std::ptr::null_mut());
        libc::raise(signum);
    }
}

fn install_abort_handlers() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = abort_signal_handler as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGHUP, &sa, std::ptr::null_mut());
    }
}

// Sends the abort line on drop unless explicitly disarmed. Covers panics and
// any path that exits the function without a clean response.
struct AbortGuard {
    fd: i32,
    armed: bool,
}

impl AbortGuard {
    fn arm(fd: i32) -> Self {
        ABORT_FD.store(fd, Ordering::SeqCst);
        Self { fd, armed: true }
    }
    fn disarm(&mut self) {
        self.armed = false;
        ABORT_FD.store(-1, Ordering::SeqCst);
    }
}

impl Drop for AbortGuard {
    fn drop(&mut self) {
        if self.armed {
            unsafe {
                libc::write(self.fd, ABORT_LINE.as_ptr().cast(), ABORT_LINE.len());
            }
            ABORT_FD.store(-1, Ordering::SeqCst);
        }
    }
}

fn read_stdin_to_string() -> std::io::Result<String> {
    let mut buf = Vec::new();
    std::io::stdin().lock().read_to_end(&mut buf)?;
    String::from_utf8(buf).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "stdin is not valid UTF-8")
    })
}

fn send_request(mut stream: &UnixStream, request: &Request) -> std::io::Result<()> {
    let body = serde_json::to_string(request)
        .map_err(|e| std::io::Error::other(format!("serialize request: {e}")))?;
    stream.write_all(body.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn read_response(reader: &mut BufReader<UnixStream>) -> std::io::Result<Response> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let resp: Response = serde_json::from_str(line.trim_end())
        .map_err(|e| std::io::Error::other(format!("parse response: {e}")))?;
    Ok(resp)
}

pub fn list(limit: usize) -> i32 {
    match transcript::list_recent(limit) {
        Ok(items) => {
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            for item in items {
                let _ = writeln!(out, "{} commands={}", item.name, item.command_count);
            }
            0
        }
        Err(err) => {
            eprintln!("rexec: failed to list transcripts: {err}");
            127
        }
    }
}

pub fn attach(color: ColorChoice) -> i32 {
    let ansi = match color {
        ColorChoice::Auto => std::io::stdout().is_terminal(),
        ColorChoice::Never => false,
        ColorChoice::Always => true,
    };
    let mut connection = match HostConnection::connect(&socket::socket_path()) {
        Ok(connection) => connection,
        Err(_) => {
            eprintln!("HOST NOT FOUND");
            return 127;
        }
    };
    let action = ClientAction::Attach { ansi };
    let body = match serde_json::to_string(&action) {
        Ok(body) => body,
        Err(err) => {
            eprintln!("rexec: cannot create attach request: {err}");
            return 127;
        }
    };
    let mut writer = connection.stream();
    if writer.write_all(body.as_bytes()).is_err()
        || writer.write_all(b"\n").is_err()
        || writer.flush().is_err()
    {
        eprintln!("rexec: failed to attach to host");
        return 127;
    }

    let mut line = String::new();
    loop {
        line.clear();
        match connection.reader().read_line(&mut line) {
            Ok(0) => return 0,
            Ok(_) => {
                let event = match serde_json::from_str::<HostEvent>(line.trim_end()) {
                    Ok(event) => event,
                    Err(err) => {
                        eprintln!("rexec: invalid attach response: {err}");
                        return 127;
                    }
                };
                match event {
                    HostEvent::Transcript { entry } => render_entry(&entry, ansi),
                }
            }
            Err(err) => {
                eprintln!("rexec: error reading attached host: {err}");
                return 127;
            }
        }
    }
}

pub fn print(name: &str, follow: bool) -> i32 {
    let path = match transcript::path_for(name) {
        Ok(p) => p,
        Err(err) => {
            eprintln!("rexec: cannot resolve transcript path: {err}");
            return 127;
        }
    };
    if !path.exists() {
        eprintln!("rexec: transcript not found: {}", path.display());
        return 127;
    }

    let mut offset = 0u64;
    match render_until_eof(&path, &mut offset) {
        Ok(()) => {}
        Err(err) => {
            eprintln!("rexec: error reading transcript: {err}");
            return 127;
        }
    }
    if !follow {
        return 0;
    }
    loop {
        std::thread::sleep(std::time::Duration::from_millis(250));
        match render_until_eof(&path, &mut offset) {
            Ok(()) => {}
            Err(err) => {
                eprintln!("rexec: error following transcript: {err}");
                return 127;
            }
        }
    }
}

fn render_until_eof(path: &Path, offset: &mut u64) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};

    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    if len <= *offset {
        return Ok(());
    }
    f.seek(SeekFrom::Start(*offset))?;
    let mut tail = Vec::new();
    f.read_to_end(&mut tail)?;

    // Render whole lines; defer any trailing partial line.
    let mut consumed = 0usize;
    while let Some(newline) = tail[consumed..].iter().position(|&b| b == b'\n') {
        let end = consumed + newline;
        let line = &tail[consumed..end];
        consumed = end + 1;
        if line.is_empty() {
            continue;
        }
        if let Ok(text) = std::str::from_utf8(line)
            && let Ok(entry) = serde_json::from_str::<TranscriptEntry>(text.trim_end())
        {
            render_entry(&entry, false);
        }
    }
    *offset += consumed as u64;
    Ok(())
}

fn render_entry(entry: &TranscriptEntry, ansi: bool) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = render_entry_to(&mut out, entry, ansi);
    let _ = out.flush();
}

fn render_entry_to(
    out: &mut impl Write,
    entry: &TranscriptEntry,
    ansi: bool,
) -> std::io::Result<()> {
    let mut header = String::new();
    if let Some(ts) = &entry.time {
        if ansi {
            header.push_str("\x1b[2m");
        }
        header.push('[');
        header.push_str(ts);
        header.push_str("] ");
        if ansi {
            header.push_str("\x1b[0m");
        }
    }
    if ansi {
        header.push_str("\x1b[1;36m");
    }
    header.push_str(&entry.whoami);
    if ansi {
        header.push_str("\x1b[0m");
    }
    header.push(':');
    if ansi {
        header.push_str("\x1b[34m");
    }
    header.push_str(&entry.dir);
    if ansi {
        header.push_str("\x1b[0m \x1b[1;33m$\x1b[0m");
    } else {
        header.push_str(" $");
    }
    header.push_str(" TO=");
    header.push_str(&entry.timeout.to_string());
    for arg in &entry.exec {
        header.push(' ');
        header.push_str(&shell_quote(arg));
    }
    writeln!(out, "{header}")?;
    out.write_all(entry.output.as_bytes())?;
    if !entry.output.ends_with('\n') {
        out.write_all(b"\n")?;
    }
    out.write_all(b"\n")?;
    Ok(())
}

fn shell_quote(arg: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(output: &str) -> TranscriptEntry {
        TranscriptEntry {
            whoami: "agent".into(),
            dir: "/tmp".into(),
            envs: BTreeMap::new(),
            exec: vec!["printf".into(), "red".into()],
            timeout: 0,
            exit: 0,
            output: output.into(),
            error: None,
            time: Some("2026-09-01T00:00:00Z".into()),
        }
    }

    #[test]
    fn attach_renderer_colors_headers_and_preserves_raw_output() {
        let mut rendered = Vec::new();
        render_entry_to(&mut rendered, &entry("\x1b[31mred\x1b[0m\n"), true).unwrap();
        assert!(rendered.starts_with(b"\x1b[2m["));
        assert!(rendered.windows(9).any(|part| part == b"\x1b[31mred\x1b"));
    }

    #[test]
    fn plain_renderer_introduces_no_ansi_sequences() {
        let mut rendered = Vec::new();
        render_entry_to(&mut rendered, &entry("red\n"), false).unwrap();
        assert!(!rendered.contains(&0x1b));
    }

    #[test]
    fn renderer_includes_requested_timeout_on_command_line() {
        let mut timed = entry("red\n");
        timed.timeout = 12;
        let mut rendered = Vec::new();
        render_entry_to(&mut rendered, &timed, false).unwrap();
        let rendered = String::from_utf8(rendered).unwrap();
        assert!(rendered.starts_with("[2026-09-01T00:00:00Z] agent:/tmp $ TO=12 printf red\n"));
    }

    #[test]
    fn renderer_includes_zero_for_unlimited_commands() {
        let mut rendered = Vec::new();
        render_entry_to(&mut rendered, &entry("red\n"), false).unwrap();
        let rendered = String::from_utf8(rendered).unwrap();
        assert!(rendered.starts_with("[2026-09-01T00:00:00Z] agent:/tmp $ TO=0 printf red\n"));
    }
}
