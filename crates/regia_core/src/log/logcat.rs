// SPDX-License-Identifier: MPL-2.0
//! Android logcat sink: each formatted `tracing` event becomes one logcat record with the event's
//! level and target (used as the logcat tag). `log` records reach here through the `log` -> `tracing` bridge.

use std::io::{self, Write};

use tracing::Metadata;
use tracing_subscriber::fmt::MakeWriter;

/// `MakeWriter` producing one [`LogcatLine`] per event.
pub struct Logcat;

/// Buffers one formatted event and hands it to logcat when dropped.
pub struct LogcatLine {
    level: log::Level,
    target: String,
    buf: Vec<u8>,
}

impl Write for LogcatLine {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for LogcatLine {
    fn drop(&mut self) {
        let text = String::from_utf8_lossy(&self.buf);
        let text = text.trim_end();
        if text.is_empty() {
            return;
        }
        android_logger::log(
            &log::Record::builder()
                .level(self.level)
                .target(&self.target)
                // android_logger takes the logcat tag from the module path.
                .module_path(Some(&self.target))
                .args(format_args!("{text}"))
                .build(),
        );
    }
}

fn level(meta: &Metadata<'_>) -> log::Level {
    match *meta.level() {
        tracing::Level::ERROR => log::Level::Error,
        tracing::Level::WARN => log::Level::Warn,
        tracing::Level::INFO => log::Level::Info,
        tracing::Level::DEBUG => log::Level::Debug,
        tracing::Level::TRACE => log::Level::Trace,
    }
}

impl<'a> MakeWriter<'a> for Logcat {
    type Writer = LogcatLine;
    fn make_writer(&'a self) -> Self::Writer {
        LogcatLine {
            level: log::Level::Info,
            target: "regia".into(),
            buf: Vec::new(),
        }
    }
    fn make_writer_for(&'a self, meta: &Metadata<'_>) -> Self::Writer {
        LogcatLine {
            level: level(meta),
            target: meta.target().into(),
            buf: Vec::new(),
        }
    }
}
