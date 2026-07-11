//! Response-body streaming with in-flight usage accounting.
//!
//! Implements three accounting stream types (SSE, NDJSON, buffered) that
//! forward upstream bytes verbatim while scanning for token usage blocks.
//! Usage observations are accumulated and recorded on `UsageRecorder::drop`,
//! covering both normal completion and client disconnect.

use std::sync::Arc;

use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::Value;
use twl_backends::energy::MeterGuard;
use twl_backends::LoadGuard;

use crate::fair_queue::FairQueueGuard;
use twl_provider::usage::{
    expects_usage, extract_usage, parse_ndjson_line, parse_sse_line, scavenge_usage,
};
use twl_store::Usage;

use crate::sink::AccountingWriter;

/// Context passed through the accounting pipeline: accounting writer, endpoint
/// path, requested model, and API key id.
#[derive(Clone)]
pub(crate) struct AccountingContext {
    /// Nonblocking writer handle for recording token usage.
    pub(crate) writer: AccountingWriter,
    /// The endpoint path for logging and usage detection.
    pub(crate) endpoint: String,
    /// Model from the request body (fallback when upstream response omits it).
    pub(crate) req_model: Arc<Option<String>>,
    /// API key id for usage attribution, or `None` for keyless.
    pub(crate) api_key_id: Option<i64>,
}

impl AccountingContext {
    /// Build a new `AccountingContext`.
    pub(crate) fn new(
        writer: AccountingWriter,
        endpoint: String,
        req_model: Arc<Option<String>>,
        api_key_id: Option<i64>,
    ) -> Self {
        Self {
            writer,
            endpoint,
            req_model,
            api_key_id,
        }
    }
}

/// Cap on buffered upstream response for usage parsing (8 MiB). Beyond this,
/// bytes are forwarded but accounting is skipped.
const MAX_ACCOUNTING_BUFFER: usize = 8 * 1024 * 1024;

/// Cap on a single un-terminated line in the parse buffer (1 MiB). Lines
/// longer than this are skipped for accounting (bytes still forwarded).
const MAX_LINE: usize = 1024 * 1024;

/// Records the last usage it was given and writes it on Drop. This is the RAII
/// mechanism that ensures usage is recorded even for interrupted requests.
/// The `Drop` implementation queues the last `Usage` (if any), then drops the
/// `MeterGuard` (which queues the energy record).
struct UsageRecorder {
    writer: AccountingWriter,
    usage: Option<Usage>,
    /// Energy meter guard; emits an energy row on drop when watts > 0.
    guard: Option<MeterGuard>,
    /// API key id for usage attribution.
    api_key_id: Option<i64>,
    /// In-flight load guard; holds per-backend concurrency slot for request
    /// lifetime. Read only via its `Drop` (releases the slot).
    _load: LoadGuard,
    /// Global constrained-resource admission slot for the response lifetime.
    _fair_queue: FairQueueGuard,
}

/// Merge cumulative streaming usage observations without discarding fields
/// that are reported only in an earlier event. Anthropic, for example, sends
/// input tokens and the serving model in `message_start`, then output tokens
/// in a later `message_delta`.
fn accumulate_usage(current: &mut Option<Usage>, next: Usage) {
    let Some(existing) = current.as_mut() else {
        *current = Some(next);
        return;
    };

    if existing.model == "unknown" {
        existing.model = next.model;
    }
    existing.input_tokens = existing.input_tokens.max(next.input_tokens);
    existing.output_tokens = existing.output_tokens.max(next.output_tokens);
    existing.cached_tokens = existing.cached_tokens.max(next.cached_tokens);
    existing.total_tokens = existing
        .total_tokens
        .max(next.total_tokens)
        .max(existing.input_tokens.saturating_add(existing.output_tokens));
}

impl UsageRecorder {
    // Constructed inline in stream functions.
}

impl Drop for UsageRecorder {
    /// Queue the last usage block (if any) and drop the meter guard. This
    /// fires when the stream ends normally or when the client disconnects.
    /// Energy is queued via the meter guard's drop (nothing when watts == 0).
    fn drop(&mut self) {
        // Queue usage if any was found.
        if let Some(mut u) = self.usage.take() {
            u.api_key_id = self.api_key_id;
            self.writer.record_usage(u);
        }
        // Drop the guard to queue the energy record (or nothing when watts == 0).
        drop(self.guard.take());
    }
}

/// Wraps an upstream SSE byte stream, forwarding chunks verbatim while
/// scanning `data:` lines for usage blocks. Usage recorded on drop.
pub fn sse_accounting_stream(
    upstream: impl futures_util::Stream<Item = reqwest::Result<Bytes>> + Unpin + Send + 'static,
    ctx: AccountingContext,
    guard: MeterGuard,
    load: LoadGuard,
    fair_queue: FairQueueGuard,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
    line_accounting_stream(upstream, ctx, parse_sse_line, guard, load, fair_queue)
}

/// Wraps an upstream NDJSON byte stream, forwarding chunks verbatim while
/// scanning each line for a usage block. Usage recorded on drop.
pub fn ndjson_accounting_stream(
    upstream: impl futures_util::Stream<Item = reqwest::Result<Bytes>> + Unpin + Send + 'static,
    ctx: AccountingContext,
    guard: MeterGuard,
    load: LoadGuard,
    fair_queue: FairQueueGuard,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
    line_accounting_stream(upstream, ctx, parse_ndjson_line, guard, load, fair_queue)
}

/// Shared machinery for line-based accounting (SSE and NDJSON). Forwards
/// bytes verbatim, feeds newline-delimited lines to `parse_line`, and
/// records the last usage seen on drop.
fn line_accounting_stream<P>(
    mut upstream: impl futures_util::Stream<Item = reqwest::Result<Bytes>> + Unpin + Send + 'static,
    ctx: AccountingContext,
    parse_line: P,
    guard: MeterGuard,
    load: LoadGuard,
    fair_queue: FairQueueGuard,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static
where
    P: Fn(&[u8], &str, &Option<String>) -> Option<Usage> + Send + 'static,
{
    let endpoint = ctx.endpoint.clone();
    let req_model = ctx.req_model.clone();
    async_stream::stream! {
        let mut recorder = UsageRecorder {
            writer: ctx.writer,
            usage: None,
            guard: Some(guard),
            api_key_id: ctx.api_key_id,
            _load: load,
            _fair_queue: fair_queue,
        };
        let mut lines = LineBuffer::new();
        let mut saw_usage = false;

        while let Some(item) = upstream.next().await {
            let chunk = match item {
                Ok(chunk) => chunk,
                Err(e) => {
                    yield Err(std::io::Error::other(e));
                    break;
                }
            };
            lines.push(&chunk, &endpoint, |line| {
                if let Some(u) = parse_line(line, &endpoint, &req_model) {
                    accumulate_usage(&mut recorder.usage, u);
                    saw_usage = true;
                }
            });
            yield Ok(chunk);
        }

        // Flush trailing data that wasn't newline-terminated.
        if let Some(u) = parse_line(lines.remainder(), &endpoint, &req_model) {
            accumulate_usage(&mut recorder.usage, u);
            saw_usage = true;
        }
        if !saw_usage {
            warn_missing_usage(&endpoint, "streamed response");
        }
        // `recorder` drops here and records if any usage was found.
    }
}

/// Wraps a non-SSE upstream byte stream, buffering up to
/// `MAX_ACCOUNTING_BUFFER` to parse a usage block when the body completes.
/// Larger bodies are streamed through unaccounted. When `parse` is false,
/// the body is forwarded without accounting.
pub fn buffered_accounting_stream(
    mut upstream: impl futures_util::Stream<Item = reqwest::Result<Bytes>> + Unpin + Send + 'static,
    ctx: AccountingContext,
    guard: MeterGuard,
    load: LoadGuard,
    fair_queue: FairQueueGuard,
    parse: bool,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
    let endpoint = ctx.endpoint.clone();
    let req_model = ctx.req_model.clone();
    async_stream::stream! {
        let mut recorder = UsageRecorder {
            writer: ctx.writer,
            usage: None,
            guard: Some(guard),
            api_key_id: ctx.api_key_id,
            _load: load,
            _fair_queue: fair_queue,
        };
        let mut body = BoundedBuffer::new();

        while let Some(item) = upstream.next().await {
            let chunk = match item {
                Ok(chunk) => chunk,
                Err(e) => {
                    yield Err(std::io::Error::other(e));
                    break;
                }
            };
            if parse {
                body.push(&chunk, &endpoint);
            }
            yield Ok(chunk);
        }

        if let Some(bytes) = body.complete().filter(|_| parse) {
            recorder.usage = finalize_buffered_usage(bytes, &endpoint, &req_model);
        }
        // `recorder` drops here and records if a usage block was found.
    }
}

/// Byte buffer that yields complete newline-delimited lines while bounding
/// the size of any single un-terminated line (1 MiB). Oversized lines enter
/// a discard state that resumes after the next newline.
struct LineBuffer {
    buf: Vec<u8>,
    /// True while discarding the remainder of a line that exceeded MAX_LINE.
    discarding: bool,
}

impl LineBuffer {
    fn new() -> Self {
        LineBuffer {
            buf: Vec::new(),
            discarding: false,
        }
    }

    /// Append `chunk` and invoke `on_line` for each complete line.
    fn push(&mut self, chunk: &[u8], endpoint: &str, mut on_line: impl FnMut(&[u8])) {
        self.buf.extend_from_slice(chunk);

        // Discarding: ignore all bytes through the next newline, then resume.
        if self.discarding {
            for i in 0..self.buf.len() {
                if self.buf[i] == b'\n' {
                    // This newline terminates the oversized line.
                    self.buf.drain(..i + 1);
                    self.discarding = false;
                    break;
                }
            }
            if self.discarding {
                // No newline yet - stay in discard state.
                // Memory bound: clear if the un-terminated tail grew too large.
                if self.buf.len() > MAX_LINE {
                    self.buf.clear();
                }
                return;
            }
            // Found a newline: fall through to normal scanning of the
            // bytes that follow it.
        }

        // Single forward scan: find all newline positions, drain once, callback
        // for each complete line, then discard the consumed prefix in one shot.
        let mut next_start = 0;
        for (i, &b) in self.buf.iter().enumerate() {
            if b == b'\n' {
                let line_len = i + 1 - next_start;
                if line_len > MAX_LINE {
                    tracing::debug!(
                        path = %endpoint,
                        "line exceeds {MAX_LINE} bytes; skipping"
                    );
                    next_start = i + 1;
                    continue;
                }
                on_line(&self.buf[next_start..=i]);
                next_start = i + 1;
            }
        }
        if next_start > 0 {
            self.buf.drain(..next_start);
        }
        // An un-terminated line this large is not a usage block we can parse, so
        // drop it (bytes are already forwarded) rather than grow unbounded.
        if self.buf.len() > MAX_LINE {
            tracing::debug!(
                path = %endpoint,
                "line exceeds {MAX_LINE} bytes without a newline; dropping from parse buffer"
            );
            self.discarding = true;
            self.buf.clear();
        }
    }

    /// Trailing bytes not yet terminated by a newline. Returns empty during
    /// discard state.
    fn remainder(&self) -> &[u8] {
        if self.discarding {
            &[]
        } else {
            &self.buf
        }
    }
}

/// Bounded accumulator for the buffered-response path. Stops buffering once
/// `MAX_ACCOUNTING_BUFFER` is exceeded; bytes still flow to the client.
struct BoundedBuffer {
    buf: Vec<u8>,
    overflowed: bool,
}

impl BoundedBuffer {
    /// Create a new `BoundedBuffer`.
    fn new() -> Self {
        BoundedBuffer {
            buf: Vec::new(),
            overflowed: false,
        }
    }

    /// Append `chunk` up to `MAX_ACCOUNTING_BUFFER` total bytes. Once the
    /// buffer overflows, subsequent chunks are silently dropped.
    fn push(&mut self, chunk: &[u8], endpoint: &str) {
        if self.overflowed {
            return;
        }
        if self.buf.len() + chunk.len() > MAX_ACCOUNTING_BUFFER {
            self.overflowed = true;
            self.buf = Vec::new(); // free it; can't reliably parse a partial body
            tracing::debug!(
                path = %endpoint,
                "response exceeds {MAX_ACCOUNTING_BUFFER} bytes; skipping usage accounting"
            );
        } else {
            self.buf.extend_from_slice(chunk);
        }
    }

    /// The complete body if it fit within the cap, else `None`.
    fn complete(&self) -> Option<&[u8]> {
        (!self.overflowed).then_some(self.buf.as_slice())
    }
}

/// Parse a fully-buffered response body for a usage block. Tries the known
/// response shapes first, then the heuristic scavenger.
fn finalize_buffered_usage(
    bytes: &[u8],
    endpoint: &str,
    req_model: &Option<String>,
) -> Option<Usage> {
    let Ok(v) = serde_json::from_slice::<Value>(bytes) else {
        tracing::debug!(path = %endpoint, "success response body was not JSON; nothing recorded");
        return None;
    };
    if let Some(u) = extract_usage(&v, endpoint, req_model) {
        return Some(u);
    }
    if let Some(u) = scavenge_usage(&v, endpoint, req_model) {
        // No known shape matched, but the heuristic found token counts. Record
        // them so drift doesn't zero out accounting - and still warn, because an
        // adapter in `proxy::usage` is likely needed for exact numbers.
        tracing::warn!(
            path = %endpoint,
            model = %u.model,
            "recorded usage via heuristic fallback; no known response shape matched \
             - upstream format may have changed, an adapter may be needed (see proxy::usage)"
        );
        return Some(u);
    }
    warn_missing_usage(endpoint, "successful response");
    None
}

/// Log a warning if a usage-bearing endpoint produced no recognizable usage
/// block; otherwise log at debug level.
fn warn_missing_usage(endpoint: &str, context: &str) {
    if expects_usage(endpoint) {
        tracing::warn!(
            path = %endpoint,
            "{context} on a usage-bearing endpoint carried no recognizable usage block; \
             upstream format may have changed - see proxy::usage"
        );
    } else {
        tracing::debug!(path = %endpoint, "{context} carried no usage block; nothing recorded");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_anthropic_message_start_and_output_delta() {
        let endpoint = "/v1/messages";
        let requested = Some("requested-alias".to_string());
        let start = br#"data: {"type":"message_start","message":{"model":"claude-serving","usage":{"input_tokens":41}}}"#;
        let delta = br#"data: {"type":"message_delta","usage":{"output_tokens":9}}"#;

        let mut usage = None;
        accumulate_usage(
            &mut usage,
            parse_sse_line(start, endpoint, &requested).expect("message_start usage"),
        );
        accumulate_usage(
            &mut usage,
            parse_sse_line(delta, endpoint, &requested).expect("message_delta usage"),
        );

        let usage = usage.expect("accumulated usage");
        assert_eq!(usage.model, "claude-serving");
        assert_eq!(usage.input_tokens, 41);
        assert_eq!(usage.output_tokens, 9);
        assert_eq!(usage.total_tokens, 50);
        assert!(expects_usage(endpoint));
    }

    fn drain_lines(buf: &mut LineBuffer, chunk: &[u8]) -> Vec<Vec<u8>> {
        let mut lines = Vec::new();
        buf.push(chunk, "test", |line| lines.push(line.to_vec()));
        lines
    }

    #[test]
    fn many_lines_in_one_chunk() {
        let mut buf = LineBuffer::new();
        let chunk: Vec<u8> = (0..10_000)
            .flat_map(|i| format!("line {i}\n").into_bytes())
            .collect();
        let lines = drain_lines(&mut buf, &chunk);
        assert_eq!(lines.len(), 10_000);
        for (i, line) in lines.iter().enumerate() {
            assert_eq!(line, format!("line {i}\n").as_bytes());
        }
        assert!(buf.remainder().is_empty());
    }

    #[test]
    fn line_split_across_chunks() {
        let mut buf = LineBuffer::new();
        let first_half = b"hel".to_vec();
        let second_half = b"lo\nworld\n".to_vec();
        let lines = drain_lines(&mut buf, &first_half);
        assert!(lines.is_empty());
        let lines = drain_lines(&mut buf, &second_half);
        assert_eq!(lines, vec![b"hello\n".to_vec(), b"world\n".to_vec()]);
        assert!(buf.remainder().is_empty());
    }

    #[test]
    fn split_utf8_bytes() {
        let mut buf = LineBuffer::new();
        // "café" in UTF-8: 63 61 66 c3 a9
        let first = b"caf".to_vec();
        let second = vec![0xc3, 0xa9, b'\n'];
        let lines = drain_lines(&mut buf, &first);
        assert!(lines.is_empty());
        let lines = drain_lines(&mut buf, &second);
        assert_eq!(lines, vec![b"caf\xc3\xa9\n".to_vec()]);
        assert!(buf.remainder().is_empty());
    }

    #[test]
    fn ordered_callbacks() {
        let mut buf = LineBuffer::new();
        let chunks = vec![b"aa\n".as_slice(), b"bb\n".as_slice(), b"cc\n".as_slice()];
        let mut lines = Vec::new();
        for chunk in chunks {
            buf.push(chunk, "test", |l| lines.push(l.to_vec()));
        }
        assert_eq!(
            lines,
            vec![b"aa\n".to_vec(), b"bb\n".to_vec(), b"cc\n".to_vec()]
        );
    }

    #[test]
    fn trailing_remainder() {
        let mut buf = LineBuffer::new();
        buf.push(b"line1\nline2\npartial", "test", |_| {});
        assert_eq!(buf.remainder(), b"partial");
        buf.push(b"more\n", "test", |l| assert_eq!(l, b"partialmore\n"));
        assert!(buf.remainder().is_empty());
    }

    #[test]
    fn oversized_newline_free_remainder_clearing() {
        let mut buf = LineBuffer::new();
        let big: Vec<u8> = std::iter::repeat_n(b'x', MAX_LINE + 1).collect();
        buf.push(&big, "test", |_| {});
        assert_eq!(buf.remainder(), b"");
    }

    #[test]
    fn remainder_empty_while_discarding() {
        let mut buf = LineBuffer::new();
        // Push data that exceeds MAX_LINE without a newline.
        buf.push(&vec![b'x'; MAX_LINE + 100], "test", |_| {});
        // discarding is set, buffer was cleared -> remainder is empty.
        assert!(buf.remainder().is_empty());
    }

    #[test]
    fn oversized_complete_line_in_single_chunk() {
        let mut buf = LineBuffer::new();
        let mut collected: Vec<Vec<u8>> = Vec::new();

        // One oversized complete line (MAX_LINE+1 bytes + \n) followed by a
        // valid usage-sized line in a single chunk.
        let mut chunk = vec![b'a'; MAX_LINE + 1];
        chunk.push(b'\n');
        chunk.extend_from_slice(b"valid\n");

        buf.push(&chunk, "test", |line| collected.push(line.to_vec()));

        assert_eq!(collected, vec![b"valid\n".to_vec()]);
        assert!(buf.remainder().is_empty());
    }

    #[test]
    fn oversized_line_fragmented_clear_resync() {
        let mut buf = LineBuffer::new();
        let mut collected: Vec<Vec<u8>> = Vec::new();

        // Build oversized line across chunks; buffer exceeds MAX_LINE without
        // a newline -> clear + discarding.
        let half = MAX_LINE / 2;
        buf.push(&vec![b'a'; half], "test", |_| {});
        buf.push(&vec![b'a'; half], "test", |_| {});
        // buf = 'a' * MAX_LINE, no newline, still ≤ MAX_LINE
        buf.push(&vec![b'a'; half + 1], "test", |_| {});
        // buf = 'a' * (MAX_LINE + half + 1) > MAX_LINE -> clear, discarding = true

        // Verify remainder() does not expose the discarded buffer.
        assert!(buf.remainder().is_empty());

        // Next chunk: the oversized-line's terminating newline lands inside
        // a fragment that resembles valid SSE data. Everything up to and
        // including that newline is discarded; a valid line after it must
        // still be parsed.
        let tail = b"data: {\"content\":\"fragment\"}\n";
        let valid = b"good_line\n";
        let chunk: Vec<u8> = tail.iter().chain(valid.iter()).copied().collect();
        buf.push(&chunk, "test", |line| collected.push(line.to_vec()));

        // Only `good_line` is parsed; the SSE-looking tail is discarded.
        assert_eq!(collected, vec![b"good_line\n".to_vec()]);
        assert!(buf.remainder().is_empty());
    }

    #[test]
    fn oversized_line_spanning_chunks() {
        let mut buf = LineBuffer::new();
        let mut collected: Vec<Vec<u8>> = Vec::new();

        // Phase 1 - oversized line terminates within the buffer
        // (remainder never exceeds MAX_LINE before its newline).
        let part1 = MAX_LINE / 2;
        buf.push(&vec![b'a'; part1], "test", |_| {});
        buf.push(&vec![b'a'; MAX_LINE / 2], "test", |_| {});
        // buffer = MAX_LINE bytes, no newline yet, remainder == MAX_LINE
        buf.push(b"\n", "test", |_| {});
        // oversized line skipped, drain(..next_start) removes everything
        assert!(buf.remainder().is_empty());

        // Valid line after the oversized one still parses.
        buf.push(b"valid1\n", "test", |line| collected.push(line.to_vec()));
        assert_eq!(collected, vec![b"valid1\n".to_vec()]);

        // Phase 2 - buffer exceeds MAX_LINE and clears *before* the
        // terminating newline arrives. The discarded tail of the next
        // chunk resembles valid SSE data; no callback must occur for it.
        // After the oversized line's newline, a valid line in the same
        // chunk must still be parsed.
        collected.clear();

        // Accumulate past MAX_LINE without a newline so the buffer clears
        // and enters the discard state.
        let half = MAX_LINE / 2;
        buf.push(&vec![b'a'; half], "test", |_| {});
        buf.push(&vec![b'a'; half], "test", |_| {});
        buf.push(&vec![b'a'; half + 1], "test", |_| {});
        // buf = 'a' * (MAX_LINE + half + 1) > MAX_LINE -> clear, discarding = true
        assert!(buf.remainder().is_empty());

        // Next chunk: the oversized-line tail (resembling valid SSE data)
        // plus the terminating newline, then a valid line.
        let tail = b"data: {\"content\":\"fragment\"}\n";
        let valid2 = b"valid2\n";
        let chunk: Vec<u8> = tail.iter().chain(valid2.iter()).copied().collect();
        buf.push(&chunk, "test", |line| collected.push(line.to_vec()));

        // Only `valid2` should be callback'd - the SSE-looking tail is
        // discarded because it belongs to the oversized line.
        assert_eq!(collected, vec![b"valid2\n".to_vec()]);
        assert!(buf.remainder().is_empty());
    }

    #[test]
    fn oversized_line_multiple_valid_after() {
        let mut buf = LineBuffer::new();
        let mut collected: Vec<Vec<u8>> = Vec::new();

        // Oversized complete line, then two valid lines
        let mut chunk = vec![b'a'; MAX_LINE + 1];
        chunk.push(b'\n');
        chunk.extend_from_slice(b"first\n");
        chunk.extend_from_slice(b"second\n");

        buf.push(&chunk, "test", |line| collected.push(line.to_vec()));

        assert_eq!(collected, vec![b"first\n".to_vec(), b"second\n".to_vec()]);
        assert!(buf.remainder().is_empty());
    }
}
