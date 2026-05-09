//! PTY-based virtual serial port for Unix / macOS.
//!
//! Creates a pty master/slave pair, sets the master to raw mode, symlinks
//! the slave tty path to the user-configured port path (e.g. /tmp/ttyBLE),
//! then bridges I/O via two background threads so the async runtime never
//! blocks on serial I/O.

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use nix::pty::openpty;
use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg};
use nix::unistd::{dup, ttyname};
use tokio::sync::mpsc;
use tracing::info;

pub struct PtyBridge {
    /// Keep fds alive for the lifetime of the bridge.
    _master: OwnedFd,
    _slave: OwnedFd,
    symlink: PathBuf,
    /// Feed data into the PTY by cloning and sending on this sender.
    write_tx: std::sync::mpsc::Sender<Vec<u8>>,
}

impl PtyBridge {
    /// Open a PTY pair, symlink the slave to `symlink`, and start I/O threads.
    ///
    /// Returns `(PtyBridge, read_rx)` where `read_rx` yields chunks that the
    /// host application wrote to the virtual serial port.
    pub fn new(symlink: &Path, mtu: usize) -> Result<(Self, mpsc::Receiver<Vec<u8>>)> {
        let result = openpty(None, None).context("openpty() failed")?;

        let slave_path = ttyname(result.slave.as_fd()).context("ttyname() failed")?;

        // Raw mode so the PTY doesn't mangle bytes (no echo, no signals, etc.)
        let mut termios = tcgetattr(result.master.as_fd()).context("tcgetattr failed")?;
        cfmakeraw(&mut termios);
        tcsetattr(result.master.as_fd(), SetArg::TCSANOW, &termios)
            .context("tcsetattr failed")?;

        if symlink.exists() {
            bail!(
                "Port \"{}\" already exists — delete it or choose a different --port",
                symlink.display()
            );
        }
        std::os::unix::fs::symlink(&slave_path, symlink).with_context(|| {
            format!(
                "symlink {} -> {} failed",
                symlink.display(),
                slave_path.display()
            )
        })?;

        info!(
            "Port endpoint created on {} -> {}",
            symlink.display(),
            slave_path.display()
        );

        let master_raw = result.master.as_raw_fd();

        // ── Reader thread: master fd → tokio channel ─────────────────────────
        // dup so this thread independently owns its fd and closes it on exit.
        let (read_tx, read_rx) = mpsc::channel::<Vec<u8>>(64);
        {
            // In nix 0.29, dup(RawFd) -> Result<RawFd>
            let read_raw = dup(master_raw).context("dup for reader failed")?;
            let read_file = unsafe { std::fs::File::from_raw_fd(read_raw) };

            std::thread::spawn(move || {
                use std::io::Read;
                let mut file = read_file;
                let mut buf = vec![0u8; mtu.max(512)];
                loop {
                    match file.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if read_tx.blocking_send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }

        // ── Writer thread: std channel → master fd ───────────────────────────
        let (write_tx, write_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        {
            let write_raw = dup(master_raw).context("dup for writer failed")?;
            let write_file = unsafe { std::fs::File::from_raw_fd(write_raw) };

            std::thread::spawn(move || {
                use std::io::Write;
                let mut file = write_file;
                while let Ok(data) = write_rx.recv() {
                    if file.write_all(&data).is_err() {
                        break;
                    }
                }
            });
        }

        Ok((
            Self {
                _master: result.master,
                _slave: result.slave,
                symlink: symlink.to_path_buf(),
                write_tx,
            },
            read_rx,
        ))
    }

    /// Clone the sender so async tasks can push data into the PTY.
    /// `std::sync::mpsc::Sender::send` is non-blocking for unbounded channels.
    pub fn write_sender(&self) -> std::sync::mpsc::Sender<Vec<u8>> {
        self.write_tx.clone()
    }

    /// Remove the symlink and drop the bridge (closes fds, stops threads).
    pub fn remove(self) -> Result<()> {
        std::fs::remove_file(&self.symlink).ok();
        info!("Serial reader and symlink removed");
        Ok(())
    }
}
