use std::ffi::CString;
use std::fmt;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};

use nix::errno::Errno;
use nix::fcntl::{OFlag, open};
use nix::libc;
use nix::pty::{ForkptyResult, Winsize, forkpty, openpty};
use nix::sys::stat::Mode;
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
    /// provided input. Write bytes here and drop to send EOF. Otherwise the
    /// child's stdin is `/dev/null`.
    pub stdin_write: Option<OwnedFd>,
}

#[derive(Debug)]
pub enum SpawnError {
    NulByte(&'static str),
    InvalidEnvName(String),
    Pipe(Errno),
    Stdin(Errno),
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
            Self::Stdin(err) => write!(f, "failed to open child stdin: {err}"),
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
    clear_env: bool,
    cwd: &str,
    stdin_provided: bool,
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

    // When input was provided, feed it through a pipe. Otherwise attach
    // /dev/null so commands cannot block waiting for input from their private
    // PTY. O_CLOEXEC keeps the original fd out of the executed process;
    // dup2(source, 0) creates the non-CLOEXEC fd 0.
    let (stdin_read, stdin_write) = stdin_fds(stdin_provided)?;

    let winsize = Winsize {
        ws_row: PTY_ROWS,
        ws_col: PTY_COLS,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let termios = make_termios()?;

    // SAFETY: forkpty is called before this function spawns any threads in the parent. The child
    // branch performs only the existing environment/setup operations (chdir, clearenv, setenv,
    // dup2, write, _exit) plus execvp on prebuilt C strings, and _exits on any failure.
    let result = unsafe { forkpty(Some(&winsize), Some(&termios)) }.map_err(SpawnError::Fork)?;

    match result {
        ForkptyResult::Parent { child, master } => {
            drop(write_fd);
            // Parent doesn't read from the child's stdin source.
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
            let stdin_read_raw = stdin_read.as_raw_fd();
            // SAFETY: we are in the post-fork child.
            unsafe {
                if libc::dup2(stdin_read_raw, 0) < 0 {
                    let errno = Errno::last() as i32;
                    write_errno(write_raw, errno);
                    libc::_exit(127);
                }
                if libc::chdir(cwd_c.as_ptr()) != 0 {
                    let errno = Errno::last() as i32;
                    write_errno(write_raw, errno);
                    libc::_exit(127);
                }
                if clear_env && libc::clearenv() != 0 {
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

fn stdin_fds(stdin_provided: bool) -> Result<(OwnedFd, Option<OwnedFd>), SpawnError> {
    if stdin_provided {
        let (r, w) = pipe2(OFlag::O_CLOEXEC).map_err(SpawnError::Pipe)?;
        Ok((r, Some(w)))
    } else {
        let r = open(
            "/dev/null",
            OFlag::O_RDONLY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(SpawnError::Stdin)?;
        Ok((r, None))
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

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::*;

    #[test]
    fn absent_stdin_is_immediate_eof() {
        let (read_fd, write_fd) = stdin_fds(false).unwrap();
        assert!(write_fd.is_none());
        let mut input = std::fs::File::from(read_fd);
        let mut bytes = Vec::new();
        input.read_to_end(&mut bytes).unwrap();
        assert!(bytes.is_empty());
    }

    #[test]
    fn provided_stdin_is_closed_after_payload() {
        let (read_fd, write_fd) = stdin_fds(true).unwrap();
        let mut output = std::fs::File::from(write_fd.unwrap());
        output.write_all(b"payload").unwrap();
        drop(output);

        let mut input = std::fs::File::from(read_fd);
        let mut bytes = Vec::new();
        input.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"payload");
    }
}
