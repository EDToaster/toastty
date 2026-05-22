use std::io;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PtyError {
    #[error("open pty failed: {0}")]
    Open(#[source] rustix::io::Errno),

    #[error("spawn child failed: {0}")]
    Spawn(#[source] io::Error),

    #[error("ioctl failed: {0}")]
    Ioctl(#[source] io::Error),

    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, PtyError>;
