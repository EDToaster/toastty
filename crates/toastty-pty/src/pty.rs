use crate::error::{PtyError, Result};
use crate::spec::{PtySpec, WinSize};
use rustix::fs::{Mode, OFlags, open};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ExitStatus, Stdio};

/// A PTY pair with a spawned child attached to the slave side.
///
/// Owns the master `OwnedFd`. Drop will best-effort kill + reap the child.
#[derive(Debug)]
pub struct Pty {
    master: OwnedFd,
    child: Child,
    size: WinSize,
}

impl Pty {
    /// Open a PTY pair, fork+exec the child attached to the slave.
    pub fn spawn(spec: &PtySpec) -> Result<Self> {
        let (master, slave) = open_pair(spec.size)?;
        let master_raw = master.as_raw_fd();
        let slave_raw = slave.as_raw_fd();

        let mut cmd = std::process::Command::new(&spec.program);
        cmd.args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        if let Some(dir) = &spec.working_dir {
            cmd.current_dir(dir);
        }

        if !spec.env.is_empty() {
            cmd.env_clear();
            for (k, v) in &spec.env {
                cmd.env(k, v);
            }
        }

        // SAFETY: child_setup only invokes async-signal-safe libc functions,
        // which is what `pre_exec` requires (it runs in the forked child
        // between fork and exec).
        unsafe {
            cmd.pre_exec(move || child_setup(slave_raw, master_raw));
        }

        let child = cmd.spawn().map_err(PtyError::Spawn)?;

        // Parent: drop its copy of the slave; child has dup'd it onto stdio.
        drop(slave);

        Ok(Self {
            master,
            child,
            size: spec.size,
        })
    }

    pub fn master_fd(&self) -> BorrowedFd<'_> {
        self.master.as_fd()
    }

    pub fn size(&self) -> WinSize {
        self.size
    }

    /// Update both the kernel's recorded size and our cached value.
    /// The kernel will send SIGWINCH to the foreground process group.
    pub fn resize(&mut self, size: WinSize) -> Result<()> {
        set_winsize(self.master.as_fd(), size)?;
        self.size = size;
        Ok(())
    }

    /// Toggle `O_NONBLOCK` on the master fd. The mio loop in `toastty-io`
    /// will normally do this; here so callers (tests, examples) can too.
    pub fn set_nonblocking(&self, nonblocking: bool) -> Result<()> {
        let fd = self.master.as_raw_fd();
        // SAFETY: master fd is valid and owned by this Pty.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(PtyError::Io(io::Error::last_os_error()));
        }
        let new_flags = if nonblocking {
            flags | libc::O_NONBLOCK
        } else {
            flags & !libc::O_NONBLOCK
        };
        // SAFETY: master fd is valid; new_flags is the modified flag set.
        if unsafe { libc::fcntl(fd, libc::F_SETFL, new_flags) } < 0 {
            return Err(PtyError::Io(io::Error::last_os_error()));
        }
        Ok(())
    }

    /// Read bytes from the PTY master. Returns the number of bytes read,
    /// or `Ok(0)` at EOF. In non-blocking mode, `WouldBlock` means "no
    /// data right now."
    pub fn read(&self, buf: &mut [u8]) -> Result<usize> {
        // SAFETY: master fd is valid; buf describes a valid mutable slice.
        let n = unsafe {
            libc::read(self.master.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len())
        };
        if n < 0 {
            return Err(PtyError::Io(io::Error::last_os_error()));
        }
        Ok(n as usize)
    }

    /// Write bytes to the PTY master. Apps inside the PTY receive them
    /// as stdin.
    pub fn write(&self, buf: &[u8]) -> Result<usize> {
        // SAFETY: master fd is valid; buf describes a valid slice.
        let n = unsafe {
            libc::write(self.master.as_raw_fd(), buf.as_ptr().cast(), buf.len())
        };
        if n < 0 {
            return Err(PtyError::Io(io::Error::last_os_error()));
        }
        Ok(n as usize)
    }

    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        Ok(self.child.try_wait()?)
    }

    pub fn wait(&mut self) -> Result<ExitStatus> {
        Ok(self.child.wait()?)
    }

    pub fn kill(&mut self) -> Result<()> {
        self.child.kill()?;
        Ok(())
    }

    pub fn child_id(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // Best-effort: signal + reap so we never leak a child.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Runs in the forked child between fork and exec. Must only call
/// async-signal-safe functions.
fn child_setup(slave_raw: i32, master_raw: i32) -> io::Result<()> {
    // SAFETY: setsid takes no arguments and is always safe to call.
    if unsafe { libc::setsid() } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: slave_raw is a valid fd inherited via fork.
    if unsafe { libc::ioctl(slave_raw, libc::TIOCSCTTY as _, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    for fd in [0, 1, 2] {
        // SAFETY: slave_raw is valid; 0/1/2 are valid target fds.
        if unsafe { libc::dup2(slave_raw, fd) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    if slave_raw > 2 {
        // SAFETY: closing a child-side fd we own.
        let _ = unsafe { libc::close(slave_raw) };
    }
    // Don't leak the parent's master into the child.
    // SAFETY: closing a child-side copy of the master fd we own.
    let _ = unsafe { libc::close(master_raw) };
    Ok(())
}

fn open_pair(size: WinSize) -> Result<(OwnedFd, OwnedFd)> {
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).map_err(PtyError::Open)?;
    grantpt(&master).map_err(PtyError::Open)?;
    unlockpt(&master).map_err(PtyError::Open)?;

    let name = ptsname(&master, Vec::new()).map_err(PtyError::Open)?;
    let path = Path::new(OsStr::from_bytes(name.as_bytes()));

    let slave = open(path, OFlags::RDWR | OFlags::NOCTTY, Mode::empty())
        .map_err(PtyError::Open)?;

    set_winsize(master.as_fd(), size)?;

    Ok((master, slave))
}

fn set_winsize(fd: BorrowedFd<'_>, size: WinSize) -> Result<()> {
    let ws = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: size.pixel_width,
        ws_ypixel: size.pixel_height,
    };
    // SAFETY: fd is valid; &ws points to a properly initialized winsize.
    let ret = unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) };
    if ret < 0 {
        return Err(PtyError::Ioctl(io::Error::last_os_error()));
    }
    Ok(())
}
