use std::ffi::CString;
use std::fmt;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};

use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::libc;
use nix::pty::{ForkptyResult, Winsize, forkpty, openpty};
use nix::sys::termios::{
    BaudRate, ControlFlags, InputFlags, LocalFlags, OutputFlags, SpecialCharacterIndices,
    cfsetspeed, tcgetattr,
};
use nix::unistd::{Pid, execvp, pipe2};

pub const PTY_ROWS: u16 = 24;
pub const PTY_COLS: u16 = 80;

pub struct Spawned {
    pub master: OwnedFd,
    pub child: Pid,
    pub errno_pipe_read: OwnedFd,
    /// Write end of a pipe attached to the child's stdin, when the caller
    /// requested one. Write bytes here and drop to send EOF.
    pub stdin_write: Option<OwnedFd>,
}

#[derive(Debug)]
pub enum SpawnError {
    NulByte(&'static str),
    InvalidEnvName(String),
    Pipe(Errno),
    Open(Errno),
    Termios(Errno),
    Fork(Errno),
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NulByte(field) => write!(f, "PTY {field} contains a NUL byte"),
            Self::InvalidEnvName(name) => {
                write!(f, "environment variable name contains '=': {name}")
            }
            Self::Pipe(err) => write!(f, "failed to open errno pipe: {err}"),
            Self::Open(err) => write!(f, "failed to open PTY: {err}"),
            Self::Termios(err) => write!(f, "failed to configure PTY termios: {err}"),
            Self::Fork(err) => write!(f, "failed to fork PTY child: {err}"),
        }
    }
}

impl std::error::Error for SpawnError {}

pub fn spawn(
    argv: &[String],
    envs: &[(String, String)],
    cwd: &str,
    with_stdin_pipe: bool,
) -> Result<Spawned, SpawnError> {
    if argv.is_empty() {
        return Err(SpawnError::NulByte("argv"));
    }

    let program = CString::new(argv[0].as_str()).map_err(|_| SpawnError::NulByte("program"))?;
    let argv_c: Vec<CString> = argv
        .iter()
        .map(|s| CString::new(s.as_str()).map_err(|_| SpawnError::NulByte("argument")))
        .collect::<Result<_, _>>()?;
    let argv_refs: Vec<&CString> = argv_c.iter().collect();

    let envs_c: Vec<(CString, CString)> = envs
        .iter()
        .map(|(k, v)| {
            if k.as_bytes().contains(&b'=') {
                return Err(SpawnError::InvalidEnvName(k.clone()));
            }
            let key = CString::new(k.as_str()).map_err(|_| SpawnError::NulByte("env name"))?;
            let value = CString::new(v.as_str()).map_err(|_| SpawnError::NulByte("env value"))?;
            Ok((key, value))
        })
        .collect::<Result<_, SpawnError>>()?;

    let cwd_c = CString::new(cwd).map_err(|_| SpawnError::NulByte("cwd"))?;

    let (read_fd, write_fd) = pipe2(OFlag::O_CLOEXEC).map_err(SpawnError::Pipe)?;

    // Optional stdin pipe: child reads from read end, parent writes to write end.
    // O_CLOEXEC keeps the child from inheriting fds it shouldn't; dup2(read, 0)
    // in the child gives fd 0 (which is not CLOEXEC) and the original cloexec fd
    // is closed automatically at execvp.
    let (stdin_read, stdin_write) = if with_stdin_pipe {
        let (r, w) = pipe2(OFlag::O_CLOEXEC).map_err(SpawnError::Pipe)?;
        (Some(r), Some(w))
    } else {
        (None, None)
    };

    let winsize = Winsize {
        ws_row: PTY_ROWS,
        ws_col: PTY_COLS,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let termios = make_termios()?;

    // SAFETY: forkpty is called before this function spawns any threads in the parent. The child
    // branch performs only async-signal-safe operations (chdir, setenv, dup2, write, _exit) plus
    // execvp on prebuilt C strings, and _exits on any failure.
    let result = unsafe { forkpty(Some(&winsize), Some(&termios)) }.map_err(SpawnError::Fork)?;

    match result {
        ForkptyResult::Parent { child, master } => {
            drop(write_fd);
            // Parent doesn't read from the stdin pipe.
            drop(stdin_read);
            Ok(Spawned {
                master,
                child,
                errno_pipe_read: read_fd,
                stdin_write,
            })
        }
        ForkptyResult::Child => {
            let write_raw = write_fd.as_raw_fd();
            let stdin_read_raw = stdin_read.as_ref().map(|fd| fd.as_raw_fd());
            // SAFETY: we are in the post-fork child.
            unsafe {
                if let Some(fd) = stdin_read_raw
                    && libc::dup2(fd, 0) < 0
                {
                    let errno = Errno::last() as i32;
                    write_errno(write_raw, errno);
                    libc::_exit(127);
                }
                if libc::chdir(cwd_c.as_ptr()) != 0 {
                    let errno = Errno::last() as i32;
                    write_errno(write_raw, errno);
                    libc::_exit(127);
                }
                for (k, v) in &envs_c {
                    if libc::setenv(k.as_ptr(), v.as_ptr(), 1) != 0 {
                        let errno = Errno::last() as i32;
                        write_errno(write_raw, errno);
                        libc::_exit(127);
                    }
                }
                let _ = execvp(&program, &argv_refs);
                let errno = Errno::last() as i32;
                write_errno(write_raw, errno);
                libc::_exit(127);
            }
        }
    }
}

unsafe fn write_errno(fd: i32, errno: i32) {
    let bytes = errno.to_le_bytes();
    let mut written = 0usize;
    while written < bytes.len() {
        let n = unsafe {
            libc::write(
                fd,
                bytes.as_ptr().add(written).cast(),
                bytes.len() - written,
            )
        };
        if n < 0 {
            if Errno::last() == Errno::EINTR {
                continue;
            }
            return;
        }
        if n == 0 {
            return;
        }
        written += n as usize;
    }
}

pub fn read_errno(fd: &OwnedFd) -> std::io::Result<Option<i32>> {
    use std::io::Read;
    let mut file = unsafe {
        let raw = libc::dup(fd.as_raw_fd());
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        std::fs::File::from_raw_fd(raw)
    };
    let mut buf = [0u8; 4];
    let mut read = 0;
    while read < buf.len() {
        match file.read(&mut buf[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    if read == 0 {
        Ok(None)
    } else if read == buf.len() {
        Ok(Some(i32::from_le_bytes(buf)))
    } else {
        Ok(None)
    }
}

use std::os::fd::FromRawFd;

fn make_termios() -> Result<nix::sys::termios::Termios, SpawnError> {
    let pair = openpty(None, None).map_err(SpawnError::Open)?;
    let mut termios = tcgetattr(pair.slave.as_fd()).map_err(SpawnError::Termios)?;

    termios.input_flags = InputFlags::empty();
    termios.output_flags = OutputFlags::empty();
    termios
        .control_flags
        .remove(ControlFlags::CSIZE | ControlFlags::PARENB);
    termios
        .control_flags
        .insert(ControlFlags::CREAD | ControlFlags::CS8);
    termios.local_flags = LocalFlags::empty();

    set_cc(&mut termios, SpecialCharacterIndices::VMIN, 1);
    set_cc(&mut termios, SpecialCharacterIndices::VTIME, 0);

    cfsetspeed(&mut termios, BaudRate::B38400).map_err(SpawnError::Termios)?;

    Ok(termios)
}

fn set_cc(
    termios: &mut nix::sys::termios::Termios,
    index: SpecialCharacterIndices,
    value: libc::cc_t,
) {
    termios.control_chars[index as usize] = value;
}
