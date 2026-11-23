// SPDX-License-Identifier: MPL-2.0
//! The local transport on targets without one (Android): `local::start` answers `Ok(None)`
//! and the client stream type exists only so shared code compiles.

use std::io;
use std::path::Path;
use std::time::Duration;

use crate::local::LocalServer;

/// No cleanup exists on this platform.
pub(crate) struct Guard;

/// No listener exists on this platform, so no address is ever this process's.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Identity;

impl Identity {
    pub(crate) fn is_current(&self, _address: &str) -> bool {
        false
    }
}

/// A connecting local stream; unreachable on this platform.
pub(crate) struct LocalStream;

impl LocalStream {
    pub(crate) fn connect(_address: &str) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no local agent transport on this platform",
        ))
    }

    pub(crate) fn set_read_timeout(&mut self, _timeout: Option<Duration>) -> io::Result<()> {
        Ok(())
    }
}

impl io::Read for LocalStream {
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "no local transport"))
    }
}

impl io::Write for LocalStream {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "no local transport"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// No local transport on this platform.
pub(crate) fn start(
    _token: String,
    _dir: &Path,
    _board: Option<crate::render_out::RenderOutBoard>,
) -> io::Result<Option<LocalServer>> {
    Ok(None)
}
