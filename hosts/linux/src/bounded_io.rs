//! Bounded incremental line reading for helper-process stdout/stderr pipes.
//!
//! `tokio::io::Lines`/`BufReader::lines()`/`next_line()` grows its internal
//! buffer without bound while scanning for a newline, so a helper process
//! (session-agent, session-launcher, audiocap, capenc) that emits an
//! enormous or unterminated line can force unbounded memory growth in the
//! reading task, before any caller-side display truncation (e.g.
//! [`crate::eventlog::bounded_diagnostic_line`]) ever gets a chance to run.
//! [`BoundedLineReader`] instead caps the buffered bytes for one line at
//! [`crate::eventlog::MAX_FORWARDED_DIAGNOSTIC_LINE_BYTES`], discarding any
//! excess bytes read before the next newline/EOF rather than buffering
//! them, and reports the loss via [`BoundedLine::truncated`] instead of
//! silently dropping it.

use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// One incrementally-read line, capped at
/// [`crate::eventlog::MAX_FORWARDED_DIAGNOSTIC_LINE_BYTES`] bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundedLine {
    /// The line's content, lossily UTF-8 decoded and capped at the byte
    /// bound (never the raw unbounded text).
    pub(crate) text: String,
    /// Set when the real line was longer than the byte bound: the excess
    /// bytes were read (to keep the stream in sync) and discarded rather
    /// than buffered, and that loss is reported here rather than silently
    /// dropped.
    pub(crate) truncated: bool,
}

/// Wraps an `AsyncBufRead` pipe (helper stdout/stderr) with bounded,
/// allocation-capped line reading.
pub(crate) struct BoundedLineReader<R> {
    inner: R,
}

impl<R> BoundedLineReader<R>
where
    R: AsyncBufRead + Unpin,
{
    pub(crate) fn new(inner: R) -> Self {
        Self { inner }
    }

    /// Reads one line, capping buffered bytes at
    /// [`crate::eventlog::MAX_FORWARDED_DIAGNOSTIC_LINE_BYTES`] regardless
    /// of the real line's length or whether a newline ever arrives. Returns
    /// `Ok(None)` at a clean EOF with nothing left to read, mirroring
    /// `Lines::next_line`'s own `None` sentinel.
    pub(crate) async fn next_bounded_line(&mut self) -> std::io::Result<Option<BoundedLine>> {
        let cap = crate::eventlog::MAX_FORWARDED_DIAGNOSTIC_LINE_BYTES;
        let mut buf: Vec<u8> = Vec::new();
        let mut truncated = false;
        let mut saw_any_bytes = false;
        loop {
            let available = self.inner.fill_buf().await?;
            if available.is_empty() {
                break; // EOF
            }
            saw_any_bytes = true;
            if let Some(newline_at) = available.iter().position(|&byte| byte == b'\n') {
                let line_part_len = newline_at;
                append_bounded(&mut buf, &available[..line_part_len], cap, &mut truncated);
                let consumed = newline_at + 1;
                self.inner.consume(consumed);
                break;
            }
            append_bounded(&mut buf, available, cap, &mut truncated);
            let consumed = available.len();
            self.inner.consume(consumed);
        }
        if !saw_any_bytes {
            return Ok(None);
        }
        // Strip a trailing `\r` for CRLF-terminated helper output, matching
        // `Lines::next_line`'s own behavior.
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
        Ok(Some(BoundedLine {
            text: String::from_utf8_lossy(&buf).into_owned(),
            truncated,
        }))
    }
}

/// Appends as much of `chunk` as fits under `cap` into `buf`, marking
/// `truncated` when any byte of `chunk` had to be discarded instead.
fn append_bounded(buf: &mut Vec<u8>, chunk: &[u8], cap: usize, truncated: &mut bool) {
    if buf.len() >= cap {
        if !chunk.is_empty() {
            *truncated = true;
        }
        return;
    }
    let room = cap - buf.len();
    let take = chunk.len().min(room);
    buf.extend_from_slice(&chunk[..take]);
    if chunk.len() > take {
        *truncated = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    fn reader(bytes: &[u8]) -> BoundedLineReader<BufReader<std::io::Cursor<Vec<u8>>>> {
        BoundedLineReader::new(BufReader::new(std::io::Cursor::new(bytes.to_vec())))
    }

    #[tokio::test]
    async fn reads_normal_lines_and_then_ends() {
        let mut reader = reader(b"hello\nworld\n");
        let first = reader.next_bounded_line().await.unwrap().unwrap();
        assert_eq!(first.text, "hello");
        assert!(!first.truncated);
        let second = reader.next_bounded_line().await.unwrap().unwrap();
        assert_eq!(second.text, "world");
        assert!(!second.truncated);
        assert!(reader.next_bounded_line().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn caps_an_enormous_unterminated_line_without_unbounded_growth() {
        // 8x the cap, never terminated by a newline: proves the buffered
        // line is bounded rather than growing with the input, and that the
        // loss is reported rather than silently dropped.
        let cap = crate::eventlog::MAX_FORWARDED_DIAGNOSTIC_LINE_BYTES;
        let huge = vec![b'a'; cap * 8];
        let mut reader = reader(&huge);
        let line = reader.next_bounded_line().await.unwrap().unwrap();
        assert_eq!(
            line.text.len(),
            cap,
            "buffered bytes must never exceed the bound"
        );
        assert!(
            line.truncated,
            "excess bytes must be reported, not silently dropped"
        );
        assert!(reader.next_bounded_line().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn caps_an_enormous_line_that_eventually_terminates_and_resumes_after() {
        let cap = crate::eventlog::MAX_FORWARDED_DIAGNOSTIC_LINE_BYTES;
        let mut huge = vec![b'b'; cap * 4];
        huge.push(b'\n');
        huge.extend_from_slice(b"next\n");
        let mut reader = reader(&huge);
        let first = reader.next_bounded_line().await.unwrap().unwrap();
        assert_eq!(first.text.len(), cap);
        assert!(first.truncated);
        let second = reader.next_bounded_line().await.unwrap().unwrap();
        assert_eq!(
            second.text, "next",
            "the stream must resync after a truncated line"
        );
        assert!(!second.truncated);
    }

    #[tokio::test]
    async fn strips_trailing_carriage_return() {
        let mut reader = reader(b"hi\r\n");
        let line = reader.next_bounded_line().await.unwrap().unwrap();
        assert_eq!(line.text, "hi");
    }

    #[tokio::test]
    async fn unterminated_final_line_at_eof_is_still_returned() {
        let mut reader = reader(b"trailing-no-newline");
        let line = reader.next_bounded_line().await.unwrap().unwrap();
        assert_eq!(line.text, "trailing-no-newline");
        assert!(!line.truncated);
        assert!(reader.next_bounded_line().await.unwrap().is_none());
    }
}
