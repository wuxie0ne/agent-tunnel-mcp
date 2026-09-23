//! Linux PTY helpers used by the executor.
//!
//! The master is kept nonblocking and registered with Tokio's `AsyncFd`; no
//! blocking terminal I/O thread is needed.  The child receives three duped
//! views of the slave, while the parent keeps one master for the merged
//! input/output stream.

use std::{
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    ptr,
    sync::Arc,
};

use tokio::io::{Interest, unix::AsyncFd};
use tokio::process::Command;

const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;

#[derive(Clone)]
pub(crate) struct Master {
    fd: Arc<AsyncFd<OwnedFd>>,
}

pub(crate) struct Pty {
    master: Master,
    slave: OwnedFd,
}

impl Pty {
    #[cfg(target_os = "linux")]
    pub(crate) fn open() -> io::Result<Self> {
        let mut master = -1;
        let mut slave = -1;
        let size = libc::winsize {
            ws_row: DEFAULT_ROWS,
            ws_col: DEFAULT_COLS,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: all pointers are either null (optional openpty outputs) or
        // point to valid writable local storage for the duration of the call.
        let result =
            unsafe { libc::openpty(&mut master, &mut slave, ptr::null_mut(), ptr::null(), &size) };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openpty returned two owned, valid descriptors on success.
        let master = unsafe { OwnedFd::from_raw_fd(master) };
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };
        set_cloexec(master.as_raw_fd())?;
        set_cloexec(slave.as_raw_fd())?;
        set_nonblocking(master.as_raw_fd())?;
        let fd = AsyncFd::new(master)?;
        Ok(Self {
            master: Master { fd: Arc::new(fd) },
            slave,
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) fn open() -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "PTY execution is supported only on Linux",
        ))
    }

    pub(crate) fn master(&self) -> Master {
        self.master.clone()
    }

    pub(crate) fn duplicate_slave(&self) -> io::Result<OwnedFd> {
        duplicate_fd(&self.slave)
    }
}

impl Master {
    pub(crate) async fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let result = self
                .fd
                .async_io(Interest::READABLE, |fd| {
                    // SAFETY: `buffer` is valid writable storage and remains
                    // borrowed for the duration of this synchronous syscall.
                    let result = unsafe {
                        libc::read(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len())
                    };
                    if result < 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(result as usize)
                    }
                })
                .await;
            match result {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => return result,
            }
        }
    }

    pub(crate) async fn write_all(&self, buffer: &[u8]) -> io::Result<()> {
        let mut written = 0;
        while written < buffer.len() {
            let result = self
                .fd
                .async_io(Interest::WRITABLE, |fd| {
                    // SAFETY: the slice is valid for the synchronous write
                    // syscall and its length is bounded by the caller.
                    let result = unsafe {
                        libc::write(
                            fd.as_raw_fd(),
                            buffer[written..].as_ptr().cast(),
                            buffer.len() - written,
                        )
                    };
                    if result < 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(result as usize)
                    }
                })
                .await;
            match result {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "PTY master accepted no bytes",
                    ));
                }
                Ok(count) => written += count,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub(crate) fn resize(&self, rows: u16, cols: u16) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            let size = libc::winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            // SAFETY: the master descriptor is owned by this object and the
            // winsize pointer is valid for the synchronous ioctl call.
            let result = unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::TIOCSWINSZ, &size) };
            if result == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (rows, cols);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "PTY resize is supported only on Linux",
            ))
        }
    }
}

/// Configure a PTY child as a session leader with the slave as its controlling
/// terminal.  This intentionally does not call `process_group(0)`: `setsid`
/// creates the same pid-named process group without the setpgid/setsid conflict.
pub(crate) fn configure_child(cmd: &mut Command) {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: this closure only calls async-signal-safe libc operations in
        // the post-fork/pre-exec child context.  The stdio descriptors have
        // already been installed, so fd 0 is the PTY slave.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

pub(crate) fn is_pty_closed(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(errno) if errno == libc::EIO
        || errno == libc::EPIPE
        || errno == libc::ENXIO
        || errno == libc::ECONNRESET)
}

fn duplicate_fd(fd: &OwnedFd) -> io::Result<OwnedFd> {
    // SAFETY: `fd` is an owned valid descriptor and dup returns a new owned
    // descriptor on success.
    let duplicate = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate == -1 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: dup returned a new descriptor owned by this caller.
        Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
    }
}

fn set_nonblocking(fd: libc::c_int) -> io::Result<()> {
    // SAFETY: fcntl operates on the valid descriptor supplied by openpty.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fcntl changes only the status flags of this owned descriptor.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn set_cloexec(fd: libc::c_int) -> io::Result<()> {
    // SAFETY: the descriptor is owned and valid; only descriptor flags change.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn pty_descriptors_do_not_leak_across_exec() {
        let pty = Pty::open().unwrap();
        for fd in [pty.slave.as_raw_fd(), pty.master.fd.as_raw_fd()] {
            assert_ne!(
                unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
        }
        let dup = pty.duplicate_slave().unwrap();
        assert_ne!(
            unsafe { libc::fcntl(dup.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
    }
}

/// Independent operator terminal; never inherits stdin/stdout as a fallback.
/// Raw mode and fd flags are restored on every ordinary return, including
/// SIGINT handled by the attach event loop. SIGKILL/crash cannot run Drop.
#[cfg(feature = "controller")]
pub(crate) struct OperatorTty {
    io: AsyncFd<OwnedFd>,
    raw: RawMode,
}

#[cfg(feature = "controller")]
struct RawMode {
    fd: OwnedFd,
    saved: libc::termios,
    flags: i32,
}

#[cfg(feature = "controller")]
impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: the fd remains owned by self and saved was returned by
        // tcgetattr for this exact terminal before entering raw mode.
        unsafe {
            libc::tcsetattr(self.fd.as_raw_fd(), libc::TCSANOW, &self.saved);
            libc::fcntl(self.fd.as_raw_fd(), libc::F_SETFL, self.flags);
        }
    }
}

#[cfg(feature = "controller")]
impl OperatorTty {
    pub(crate) fn open() -> io::Result<Self> {
        // A daemon with a borrowed /dev/tty must not silently become an
        // "operator"; require its own inherited interactive standard fds.
        if unsafe { libc::isatty(0) } != 1 || unsafe { libc::isatty(1) } != 1 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "attach requires interactive stdin/stdout and /dev/tty",
            ));
        }
        // SAFETY: a fixed NUL-terminated path and flags; fd ownership is
        // transferred to OwnedFd only on success.
        let fd = unsafe {
            libc::open(
                c"/dev/tty".as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut saved = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(fd.as_raw_fd(), saved.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let saved = unsafe { saved.assume_init() };
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        let guard = RawMode { fd, saved, flags };
        let mut raw = guard.saved;
        unsafe {
            libc::cfmakeraw(&mut raw);
        }
        if unsafe { libc::tcsetattr(guard.fd.as_raw_fd(), libc::TCSANOW, &raw) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe {
            libc::fcntl(
                guard.fd.as_raw_fd(),
                libc::F_SETFL,
                flags | libc::O_NONBLOCK,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let io = AsyncFd::new(guard.fd.try_clone()?)?;
        Ok(Self { io, raw: guard })
    }

    pub(crate) async fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        self.io
            .async_io(Interest::READABLE, |fd| {
                let size =
                    unsafe { libc::read(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
                if size < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(size as usize)
                }
            })
            .await
    }

    pub(crate) async fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        let mut offset = 0;
        while offset < bytes.len() {
            let size = self
                .io
                .async_io(Interest::WRITABLE, |fd| {
                    let size = unsafe {
                        libc::write(
                            fd.as_raw_fd(),
                            bytes[offset..].as_ptr().cast(),
                            bytes.len() - offset,
                        )
                    };
                    if size < 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(size as usize)
                    }
                })
                .await?;
            if size == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "operator TTY closed",
                ));
            }
            offset += size;
        }
        Ok(())
    }

    pub(crate) fn size(&self) -> Option<(u16, u16)> {
        let mut size = libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        if unsafe { libc::ioctl(self.raw.fd.as_raw_fd(), libc::TIOCGWINSZ, &mut size) } == 0
            && size.ws_row > 0
            && size.ws_col > 0
        {
            Some((size.ws_row.min(1000), size.ws_col.min(1000)))
        } else {
            None
        }
    }
}
