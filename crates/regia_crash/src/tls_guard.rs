// SPDX-License-Identifier: MPL-2.0
//! Guard for the `https://` response parser (`tls` feature).
//!
//! ureq 2.12.1 slices a response header value at `name.len() + 1`. A header line with no `:` whose
//! whole text is a token (`Content-Length`, `Connection`, `Content-Encoding`, `Transfer-Encoding`)
//! therefore panics inside `send_bytes`, and the shipping profile is `panic = "abort"`: a hostile or
//! broken collector could crash the game on every launch (the report is only deleted after a 2xx).
//!
//! [`guarded_tls_connector`] wraps a rustls client config so the decrypted response stream is
//! filtered before ureq sees it: in the response head, a field line without a colon gets one
//! appended (`Content-Length` becomes `Content-Length:`, an empty value ureq parses normally).
//! Body bytes pass through untouched. The crate's agents never pool connections, so one stream
//! carries exactly one response (plus any `1xx` interim heads); a host agent that does pool
//! them stays guarded, because each request written on the stream re-arms the head filter.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use ureq::rustls;
use ureq::{ReadWrite, TlsConnector};

/// Longest response head line accepted; a longer one fails the read rather than buffering unboundedly.
const MAX_HEAD_LINE: usize = 64 * 1024;

/// Incremental rewriter of a response head. Pure and I/O-free so it is unit-testable.
#[derive(Debug, Default)]
pub(crate) struct HeadFilter {
    /// The partial line being collected (head state only).
    line: Vec<u8>,
    /// Lines already completed in the current head (0 = the next line is the status line).
    lines_done: usize,
    /// Whether the current head's status line was a `1xx` interim response.
    interim: bool,
    /// Set once the final head's blank line has been seen; everything after is body.
    body: bool,
}

impl HeadFilter {
    /// Whether the head is fully consumed and the rest of the stream is body.
    pub(crate) fn in_body(&self) -> bool {
        self.body
    }

    /// Feeds `input`, appending the rewritten bytes to `out`.
    pub(crate) fn push(&mut self, input: &[u8], out: &mut Vec<u8>) -> io::Result<()> {
        for (i, &byte) in input.iter().enumerate() {
            if self.body {
                out.extend_from_slice(&input[i..]);
                return Ok(());
            }
            self.line.push(byte);
            if byte == b'\n' {
                self.finish_line(out);
            } else if self.line.len() > MAX_HEAD_LINE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "response header line too long",
                ));
            }
        }
        Ok(())
    }

    /// Flushes a partial line unchanged at end of stream.
    pub(crate) fn finish(&mut self, out: &mut Vec<u8>) {
        out.append(&mut self.line);
    }

    fn finish_line(&mut self, out: &mut Vec<u8>) {
        let mut line = std::mem::take(&mut self.line);
        let mut end = line.len() - 1; // drop '\n'
        if end > 0 && line[end - 1] == b'\r' {
            end -= 1;
        }
        let content_empty = end == 0;
        if content_empty {
            out.extend_from_slice(&line);
            if self.interim {
                // A 1xx head is followed by another head.
                self.lines_done = 0;
                self.interim = false;
            } else {
                self.body = true;
            }
            return;
        }
        if self.lines_done == 0 {
            // Status line: "HTTP/1.1 100 Continue" — interim when the code starts with '1'.
            self.interim = line[..end]
                .split(|b| *b == b' ')
                .nth(1)
                .is_some_and(|code| code.len() == 3 && code[0] == b'1');
        } else if !line[..end].contains(&b':') {
            line.insert(end, b':');
        }
        self.lines_done += 1;
        out.extend_from_slice(&line);
    }
}

/// A stream whose reads pass through a [`HeadFilter`].
struct GuardedStream {
    inner: Box<dyn ReadWrite>,
    filter: HeadFilter,
    pending: Vec<u8>,
    cursor: usize,
    eof: bool,
}

impl std::fmt::Debug for GuardedStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuardedStream").finish_non_exhaustive()
    }
}

impl ReadWrite for GuardedStream {
    fn socket(&self) -> Option<&TcpStream> {
        self.inner.socket()
    }
}

impl Read for GuardedStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.cursor < self.pending.len() {
                let n = (self.pending.len() - self.cursor).min(buf.len());
                buf[..n].copy_from_slice(&self.pending[self.cursor..self.cursor + n]);
                self.cursor += n;
                return Ok(n);
            }
            self.pending.clear();
            self.cursor = 0;
            if self.filter.in_body() {
                return self.inner.read(buf);
            }
            if self.eof {
                return Ok(0);
            }
            let mut chunk = [0u8; 4096];
            let n = self.inner.read(&mut chunk)?;
            if n == 0 {
                self.eof = true;
                self.filter.finish(&mut self.pending);
            } else {
                self.filter.push(&chunk[..n], &mut self.pending)?;
            }
        }
    }
}

impl Write for GuardedStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // HTTP/1.1 without pipelining: a request written after a response's body began means the
        // next bytes read are a new response head. Re-arm the filter, so a pooled connection's
        // later heads are repaired too (the guard used to filter only the first one;
        // audit-concurrency-1103 crash M3).
        if self.filter.in_body() && !buf.is_empty() {
            self.filter = HeadFilter::default();
        }
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// The connector returned by [`guarded_tls_connector`].
pub struct GuardedConnector {
    config: Option<Arc<rustls::ClientConfig>>,
}

impl TlsConnector for GuardedConnector {
    fn connect(&self, dns_name: &str, io: Box<dyn ReadWrite>) -> Result<Box<dyn ReadWrite>, ureq::Error> {
        let Some(config) = &self.config else {
            return Err(io::Error::other("no TLS provider available").into());
        };
        let inner = TlsConnector::connect(config, dns_name, io)?;
        Ok(Box::new(GuardedStream {
            inner,
            filter: HeadFilter::default(),
            pending: Vec::new(),
            cursor: 0,
            eof: false,
        }))
    }
}

/// A ureq TLS connector that uses `config` and repairs colon-less response header lines that would
/// otherwise panic ureq 2.12.1 (see the module docs). Pass it to
/// `ureq::AgentBuilder::tls_connector` for an agent given to [`crate::transport::post_with_agent`];
/// agents from [`crate::transport::build_agent`] already use it with the Mozilla root list.
pub fn guarded_tls_connector(config: Arc<rustls::ClientConfig>) -> Arc<GuardedConnector> {
    Arc::new(GuardedConnector { config: Some(config) })
}

/// The default connector: rustls with the `ring` provider, TLS 1.2/1.3, Mozilla roots, guarded.
/// If the provider cannot be set up (it always can with `ring`), every connection fails with an
/// error instead of falling back to an unguarded ureq connector.
pub(crate) fn default_connector() -> Arc<GuardedConnector> {
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    match rustls::ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions() {
        Ok(builder) => guarded_tls_connector(Arc::new(
            builder.with_root_certificates(roots).with_no_client_auth(),
        )),
        Err(_) => Arc::new(GuardedConnector { config: None }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[u8]]) -> Vec<u8> {
        let mut f = HeadFilter::default();
        let mut out = Vec::new();
        for c in chunks {
            f.push(c, &mut out).expect("push");
        }
        f.finish(&mut out);
        out
    }

    #[test]
    fn a_colonless_field_line_gets_a_colon() {
        let out =
            run(&[b"HTTP/1.1 200 OK\r\nContent-Length\r\nConnection: close\r\n\r\nbody Content-Length\r\n"]);
        assert_eq!(
            out,
            b"HTTP/1.1 200 OK\r\nContent-Length:\r\nConnection: close\r\n\r\nbody Content-Length\r\n"
        );
    }

    #[test]
    fn splitting_the_input_anywhere_gives_the_same_output() {
        let whole: &[u8] = b"HTTP/1.1 204 No Content\nTransfer-Encoding\nX-A: b\n\ntail";
        let want = run(&[whole]);
        for cut in 0..whole.len() {
            assert_eq!(run(&[&whole[..cut], &whole[cut..]]), want, "cut at {cut}");
        }
        assert_eq!(
            want,
            b"HTTP/1.1 204 No Content\nTransfer-Encoding:\nX-A: b\n\ntail"
        );
    }

    #[test]
    fn an_interim_head_is_followed_by_a_second_filtered_head() {
        let out = run(&[b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nConnection\r\n\r\n"]);
        assert_eq!(
            out,
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nConnection:\r\n\r\n"
        );
    }

    #[test]
    fn a_status_line_and_valid_headers_are_untouched() {
        let input: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(run(&[input]), input);
    }

    #[test]
    fn an_overlong_head_line_is_an_error_not_a_buffer() {
        let mut f = HeadFilter::default();
        let mut out = Vec::new();
        let long = vec![b'a'; MAX_HEAD_LINE + 1];
        assert!(f.push(&long, &mut out).is_err());
    }

    #[test]
    fn a_truncated_head_is_flushed_unchanged_at_eof() {
        assert_eq!(
            run(&[b"HTTP/1.1 200 OK\r\nContent-Len"]),
            b"HTTP/1.1 200 OK\r\nContent-Len"
        );
    }
}
