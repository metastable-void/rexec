use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::cli::RunArgs;
use crate::protocol::{ERROR_NOT_FOUND, Request, Response, TranscriptEntry};
use crate::socket;
use crate::transcript;

pub fn check_host() -> i32 {
    match UnixStream::connect(socket::socket_path()) {
        Ok(stream) => {
            drop(stream);
            println!("HOST RUNNING");
            0
        }
        Err(_) => {
            println!("HOST NOT FOUND");
            127
        }
    }
}

pub fn run(args: RunArgs) -> i32 {
    let stream = match UnixStream::connect(socket::socket_path()) {
        Ok(s) => s,
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

    let request = Request {
        whoami: args.whoami,
        dir: dir_str,
        envs,
        exec: args.argv.clone(),
    };

    if let Err(err) = send_request(&stream, &request) {
        eprintln!("rexec: failed to send request: {err}");
        return 127;
    }

    let response = match read_response(&stream) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("rexec: failed to read response: {err}");
            return 127;
        }
    };

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

fn send_request(mut stream: &UnixStream, request: &Request) -> std::io::Result<()> {
    let body = serde_json::to_string(request)
        .map_err(|e| std::io::Error::other(format!("serialize request: {e}")))?;
    stream.write_all(body.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn read_response(stream: &UnixStream) -> std::io::Result<Response> {
    let mut reader = BufReader::new(stream);
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
            render_entry(&entry);
        }
    }
    *offset += consumed as u64;
    Ok(())
}

fn render_entry(entry: &TranscriptEntry) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut header = String::new();
    if let Some(ts) = &entry.time {
        header.push('[');
        header.push_str(ts);
        header.push_str("] ");
    }
    header.push_str(&entry.whoami);
    header.push(':');
    header.push_str(&entry.dir);
    header.push_str(" $");
    for arg in &entry.exec {
        header.push(' ');
        header.push_str(&shell_quote(arg));
    }
    let _ = writeln!(out, "{header}");
    let _ = out.write_all(entry.output.as_bytes());
    if !entry.output.ends_with('\n') {
        let _ = out.write_all(b"\n");
    }
    let _ = out.write_all(b"\n");
    let _ = out.flush();
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

