//! Secret-redacting `tracing` writer wrapper.
//!
//! The host fans tracing output to stderr and (optionally) a rolling
//! `host.log` file. A provider error body, a misrouted env value, or an
//! echoed `Authorization` header can otherwise land a live key in
//! `copperclaw.log` — which operators routinely `cat`, paste into issues,
//! and ship to log aggregators. We interpose a [`MakeWriter`] that scrubs
//! every formatted log line through [`copperclaw_runner::redact_secrets`]
//! before it reaches the underlying writer, so the redaction is enforced at
//! the sink regardless of which call site logged the value.
//!
//! Wrapping the *writer* rather than every call site is deliberate: a single
//! choke point can't be forgotten by a future log statement, and it covers
//! third-party crates' log output too.

use std::io::{self, Write};

use tracing_subscriber::fmt::MakeWriter;

/// A [`MakeWriter`] that redacts secrets out of each formatted log line
/// before delegating to the inner `MakeWriter`.
#[derive(Clone, Copy, Debug)]
pub struct RedactingMakeWriter<M> {
    inner: M,
}

impl<M> RedactingMakeWriter<M> {
    /// Wrap `inner` so every line it would write is scrubbed first.
    pub fn new(inner: M) -> Self {
        Self { inner }
    }
}

impl<'a, M> MakeWriter<'a> for RedactingMakeWriter<M>
where
    M: MakeWriter<'a>,
{
    type Writer = RedactingWriter<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter {
            inner: self.inner.make_writer(),
        }
    }

    fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> Self::Writer {
        RedactingWriter {
            inner: self.inner.make_writer_for(meta),
        }
    }
}

/// Writer that scrubs each `write` through the redactor before forwarding it
/// to the wrapped writer.
///
/// `tracing-subscriber`'s `fmt` layer formats a whole event into one buffer
/// and issues a single `write_all` per event, so redacting per-`write` call
/// scrubs each log line as a unit — secrets are never split across two
/// `write` calls in practice. A redacted byte run never exceeds the input
/// length, so the byte count we report back to the caller is the *original*
/// `buf.len()` (what it asked us to consume), keeping the `Write` contract
/// intact even though we emitted fewer bytes downstream.
pub struct RedactingWriter<W> {
    inner: W,
}

impl<W: Write> Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Non-UTF-8 bytes can't carry a text key shape we recognise; pass
        // them through untouched rather than lossily re-encoding.
        match std::str::from_utf8(buf) {
            Ok(s) => {
                let scrubbed = copperclaw_runner::redact_secrets(s);
                self.inner.write_all(scrubbed.as_bytes())?;
                // Report the caller's full length as consumed: we wrote a
                // (possibly shorter) redacted form of exactly these bytes.
                Ok(buf.len())
            }
            Err(_) => self.inner.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    /// A `MakeWriter` that appends everything written to a shared buffer so
    /// the test can inspect what actually reached the sink.
    #[derive(Clone)]
    struct VecMaker(Arc<Mutex<Vec<u8>>>);

    struct VecWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for VecWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for VecMaker {
        type Writer = VecWriter;
        fn make_writer(&'a self) -> Self::Writer {
            VecWriter(Arc::clone(&self.0))
        }
    }

    #[test]
    fn scrubs_key_before_inner_writer_sees_it() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let maker = RedactingMakeWriter::new(VecMaker(Arc::clone(&sink)));
        let mut w = maker.make_writer();
        w.write_all(b"using key sk-abcdEF0123456789abcdEF0123456789 now\n")
            .unwrap();
        let got = String::from_utf8(sink.lock().unwrap().clone()).unwrap();
        assert!(!got.contains("sk-abcdEF"), "raw key reached sink: {got}");
        assert!(
            got.contains(copperclaw_runner::REDACTED),
            "no redaction: {got}"
        );
    }

    #[test]
    fn passes_ordinary_lines_through_unchanged() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let maker = RedactingMakeWriter::new(VecMaker(Arc::clone(&sink)));
        let mut w = maker.make_writer();
        let line = b"INFO copperclaw started (pid 4242, socket cclaw.sock)\n";
        w.write_all(line).unwrap();
        let got = sink.lock().unwrap().clone();
        assert_eq!(got, line);
    }

    #[test]
    fn write_reports_full_input_len_consumed() {
        // Even though the redacted form is shorter, `write` must report the
        // caller's full `buf.len()` as consumed or `write_all` loops forever.
        let sink = Arc::new(Mutex::new(Vec::new()));
        let maker = RedactingMakeWriter::new(VecMaker(Arc::clone(&sink)));
        let mut w = maker.make_writer();
        let buf = b"Bearer abc123DEF456ghi789JKL0 trailing";
        let n = w.write(buf).unwrap();
        assert_eq!(n, buf.len());
    }
}
