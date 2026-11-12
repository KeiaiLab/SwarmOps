//! The host's side of one native session (native sync design §2.1, §2.5): a [`Link`] sends a
//! request line and the chunks it announces, and reads the answer line; the answer's chunks are
//! then read one at a time, each checked as it arrives ([`Answer::next_chunk`]).
//!
//! ```text
//! host                                  peer (serve --stdio)
//!   call(line, upload) ── line \n upload ─▶
//!                      ◀─ envelope line \n ── chunk 1 … chunk n   (n, bytes: the line's result)
//!   Answer::next_chunk  ─▶ one chunk's objects, ids checked; at most one chunk held
//! ```
//!
//! A peer silent for [`WIRE_STALL_SECS`] ends the session (`E_REMOTE_UNREACHABLE`, `timeout`);
//! so does a peer that closes it or answers what the wire cannot read, and every later call is
//! refused as `disconnected`. The link counts the most bytes it held at once
//! ([`StreamLink::peak_bytes`]).

use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::Duration;

use serde_json::Value;
use swarmops_api::{MAX_WIRE_LINE, OpError, WIRE_STALL_SECS, WireChunks};
use swarmops_core::{Hashed, bytes};

use super::chunk::{self, Takes};
use super::{Cause, refused, unreachable};

/// The most bytes the reading thread hands over at once.
const PIECE: usize = 64 * 1024;

/// The end of a line.
const NEWLINE: u8 = b'\n';

/// One session with a peer that answers the native wire.
pub trait Link {
    /// Sends `line`, a request without its newline, and `upload`, the chunks it announces; the
    /// answer's envelope line, its chunks left to read from the [`Answer`].
    fn call(&mut self, line: &[u8], upload: &[u8]) -> Result<Answer<'_>, OpError>;
}

/// One answer: its envelope line, and the chunks the line announces, read one at a time.
///
/// Chunks left unread are skipped by the next call.
#[non_exhaustive]
pub struct Answer<'a> {
    line: Vec<u8>,
    session: &'a mut Session,
}

impl Answer<'_> {
    /// The envelope line, without its newline.
    pub fn line(&self) -> &[u8] {
        &self.line
    }

    /// The next chunk's objects, each a blob or a changeset whose bytes hash to its id; `None`
    /// once every chunk the line announced is read. A chunk refused ends the session.
    pub fn next_chunk(&mut self) -> Result<Option<Vec<Hashed>>, OpError> {
        let session = &mut *self.session;
        if session.left.chunks == 0 {
            return Ok(None);
        }

        // 1. One chunk, checked as it is read; a refused one leaves the stream unreadable.
        let read = chunk::read_chunk(&mut session.input, session.left, Takes::BlobsAndChangesets);
        let objects = read.inspect_err(|_| session.broken = true)?;

        // 2. What is left of the announcement, and the most held: the line and this chunk.
        let len = chunk::chunk_len(&objects);
        session.left = WireChunks::new(session.left.chunks - 1, session.left.bytes - len);
        session.hold(bytes::to_u64(self.line.len()) + len);
        Ok(Some(objects))
    }
}

/// A [`Link`] over a byte stream each way: the child's stdio of an ssh command, or a test's
/// pipes.
#[non_exhaustive]
pub struct StreamLink {
    output: Box<dyn Write + Send>,
    session: Session,
}

impl StreamLink {
    /// A link that writes requests to `output` and reads answers from `input`, which a thread
    /// of its own reads as bytes come, so that a silent peer ends at the stall deadline.
    pub fn new(input: impl Read + Send + 'static, output: impl Write + Send + 'static) -> Self {
        let (sender, pieces) = mpsc::sync_channel(1);
        thread::spawn(move || pump(input, &sender));
        let input = Pipe {
            pieces,
            piece: Vec::new(),
            at: 0,
            stall: Duration::from_secs(WIRE_STALL_SECS),
            ended: false,
        };
        let session = Session {
            input,
            left: WireChunks::new(0, 0),
            peak: 0,
            broken: false,
        };
        StreamLink {
            output: Box::new(output),
            session,
        }
    }

    /// The most bytes the link held at once: an answer line and one chunk.
    pub fn peak_bytes(&self) -> u64 {
        self.session.peak
    }

    /// The link with a stall deadline of `stall` instead of [`WIRE_STALL_SECS`].
    #[cfg(test)]
    fn with_stall(mut self, stall: Duration) -> Self {
        self.session.input.stall = stall;
        self
    }
}

impl Link for StreamLink {
    fn call(&mut self, line: &[u8], upload: &[u8]) -> Result<Answer<'_>, OpError> {
        // 1. The request's own line, checked before a byte is sent.
        if line.len() > MAX_WIRE_LINE {
            return Err(too_long(Side::Request));
        }
        if line.contains(&NEWLINE) {
            let message = "a request line holds a line break";
            return Err(refused("line_break", message));
        }
        let session = &mut self.session;
        if session.broken {
            return Err(unreachable(Cause::Disconnected));
        }

        // 2. The chunks the last answer left unread, skipped; then the request and its upload.
        session.skip_left().inspect_err(|_| session.broken = true)?;
        let sent = send(&mut self.output, line, upload);
        sent.inspect_err(|_| session.broken = true)?;

        // 3. The answer's line, and the chunks it announces.
        let answer = session.input.read_line();
        let answer = answer.inspect_err(|_| session.broken = true)?;
        let announced = announced(&answer);
        session.left = announced.inspect_err(|_| session.broken = true)?;
        session.hold(bytes::to_u64(answer.len()));
        Ok(Answer {
            line: answer,
            session,
        })
    }
}

/// Which line passed [`MAX_WIRE_LINE`].
#[derive(Clone, Copy)]
enum Side {
    /// The request a caller gave.
    Request,
    /// The answer the peer sent.
    Answer,
}

/// `E_INVALID_ARG`, `line_too_long`, of the `side` line.
fn too_long(side: Side) -> OpError {
    let side = match side {
        Side::Request => "a request",
        Side::Answer => "an answer",
    };
    let message = format!("{side} line past {MAX_WIRE_LINE} bytes");
    refused("line_too_long", message)
}

/// Writes `line`, its newline and `upload`, and flushes them.
fn send(output: &mut dyn Write, line: &[u8], upload: &[u8]) -> Result<(), OpError> {
    let mut write = || -> io::Result<()> {
        output.write_all(line)?;
        output.write_all(&[NEWLINE])?;
        output.write_all(upload)?;
        output.flush()
    };
    write().map_err(|e| unreachable(Cause::of(&e)))
}

/// The chunks an answer `line` announces: a successful envelope whose result counts `chunks`
/// and `bytes` (`wire_get`'s), else none. A line that is no JSON object leaves the stream
/// unreadable: whether chunks follow it is unknown.
fn announced(line: &[u8]) -> Result<WireChunks, OpError> {
    // 1. The envelope.
    let Ok(Value::Object(envelope)) = serde_json::from_slice::<Value>(line) else {
        return Err(refused(
            "answer_line",
            "an answer line that is no JSON object",
        ));
    };
    if envelope.get("ok") != Some(&Value::Bool(true)) {
        return Ok(WireChunks::new(0, 0));
    }

    // 2. Its result's count of chunks and their bytes, both or neither, consistent.
    let result = envelope.get("result");
    let field = |name: &str| {
        result
            .and_then(|result| result.get(name))
            .map(Value::as_u64)
    };
    let counts = match (field("chunks"), field("bytes")) {
        (None, None) => Some((0, 0)),
        (Some(Some(chunks)), Some(Some(bytes))) => u32::try_from(chunks).ok().zip(Some(bytes)),
        _ => None,
    };
    let consistent = |&(chunks, bytes): &(u32, u64)| (chunks == 0) == (bytes == 0);
    let Some((chunks, bytes)) = counts.filter(consistent) else {
        return Err(refused(
            "answer_line",
            "an answer's chunk counts do not agree",
        ));
    };
    Ok(WireChunks::new(chunks, bytes))
}

/// Hands `input`'s bytes to `sender` as they come, until the stream ends, fails, or the link
/// is gone.
fn pump(mut input: impl Read, sender: &SyncSender<io::Result<Vec<u8>>>) {
    loop {
        let mut piece = vec![0; PIECE];
        let piece = match input.read(&mut piece) {
            Ok(0) => return,
            Ok(n) => {
                piece.truncate(n);
                Ok(piece)
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => Err(e),
        };
        let failed = piece.is_err();
        if sender.send(piece).is_err() || failed {
            return;
        }
    }
}

/// What a link reads, and what it knows of it.
struct Session {
    input: Pipe,
    /// What is left of the chunks the last answer announced.
    left: WireChunks,
    /// The most bytes held at once.
    peak: u64,
    /// Whether the stream is past a point the wire can read on from.
    broken: bool,
}

impl Session {
    /// Counts `held` bytes held at once.
    fn hold(&mut self, held: u64) {
        self.peak = self.peak.max(held);
    }

    /// Reads past the chunks the last answer left unread.
    fn skip_left(&mut self) -> Result<(), OpError> {
        let left = self.left.bytes;
        let skipped = io::copy(&mut (&mut self.input).take(left), &mut io::sink());
        let skipped = skipped.map_err(|e| unreachable(Cause::of(&e)))?;
        if skipped != left {
            return Err(unreachable(Cause::Disconnected));
        }
        self.left = WireChunks::new(0, 0);
        Ok(())
    }
}

/// The peer's bytes, as the reading thread hands them over: a read waits at most `stall`.
struct Pipe {
    pieces: Receiver<io::Result<Vec<u8>>>,
    piece: Vec<u8>,
    at: usize,
    stall: Duration,
    ended: bool,
}

impl Pipe {
    /// The bytes handed over and not yet read, after waiting for more if there are none; empty
    /// once the stream ended.
    fn fill(&mut self) -> io::Result<&[u8]> {
        while self.at == self.piece.len() && !self.ended {
            match self.pieces.recv_timeout(self.stall) {
                Ok(Ok(piece)) => {
                    self.piece = piece;
                    self.at = 0;
                }
                Ok(Err(e)) => {
                    self.ended = true;
                    return Err(e);
                }
                Err(RecvTimeoutError::Timeout) => return Err(io::ErrorKind::TimedOut.into()),
                Err(RecvTimeoutError::Disconnected) => self.ended = true,
            }
        }
        Ok(&self.piece[self.at..])
    }

    /// One line without its newline, at most [`MAX_WIRE_LINE`] bytes: a longer one is refused
    /// once that many bytes came, a stream that ends first is the peer gone.
    fn read_line(&mut self) -> Result<Vec<u8>, OpError> {
        let mut line = Vec::new();
        loop {
            // 1. The bytes at hand, up to a newline.
            let at_hand = self.fill().map_err(|e| unreachable(Cause::of(&e)))?;
            if at_hand.is_empty() {
                return Err(unreachable(Cause::Disconnected));
            }
            let end = at_hand.iter().position(|&b| b == NEWLINE);
            let taken = end.unwrap_or(at_hand.len());
            line.extend_from_slice(&at_hand[..taken]);
            self.at += taken;

            // 2. The line's bound, then its end.
            if line.len() > MAX_WIRE_LINE {
                return Err(too_long(Side::Answer));
            }
            if end.is_some() {
                self.at += 1;
                return Ok(line);
            }
        }
    }
}

impl Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let at_hand = self.fill()?;
        let n = at_hand.len().min(buf.len());
        buf[..n].copy_from_slice(&at_hand[..n]);
        self.at += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use serde_json::json;
    use swarmops_api::ErrorCode;
    use swarmops_core::{ObjectKind, hash_object};

    use super::*;

    /// A writer whose bytes a test reads afterwards.
    #[derive(Clone, Default)]
    struct Sent(Arc<Mutex<Vec<u8>>>);

    impl Write for Sent {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Sent {
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
    }

    /// The answer line of a `wire_get` announcing `chunks` of `bytes`.
    fn get_line(chunks: u32, bytes: u64) -> Vec<u8> {
        let result = json!({"chunks": chunks, "bytes": bytes});
        let envelope =
            json!({"ok": true, "op": "wire_get", "api": 1, "result": result, "meta": {}});
        let mut line = serde_json::to_vec(&envelope).unwrap();
        line.push(NEWLINE);
        line
    }

    /// `count` chunks of one blob of `len` bytes each, distinct.
    fn chunks(count: u8, len: usize) -> Vec<Vec<u8>> {
        (0..count)
            .map(|n| {
                let payload = vec![n; len];
                let id = hash_object(ObjectKind::Blob, &payload);
                let mut out = Vec::new();
                chunk::write_chunk(&mut out, &[(ObjectKind::Blob, id, &payload)]).unwrap();
                out
            })
            .collect()
    }

    /// The refusal's `details` entry `key`.
    fn detail<'e>(refused: &'e OpError, key: &str) -> &'e str {
        refused.details[key].as_str().unwrap()
    }

    /// Three chunks of 256 KiB are read one at a time: the link held no more than the line and
    /// one chunk, and the objects come in order.
    #[test]
    fn the_link_buffers_at_most_one_chunk() {
        // 1. The answer: its line, then three chunks.
        let chunks = chunks(3, 256 * 1024);
        let total: usize = chunks.iter().map(Vec::len).sum();
        let mut answer = get_line(3, bytes::to_u64(total));
        let line_len = bytes::to_u64(answer.len() - 1);
        answer.extend(chunks.concat());

        // 2. The call, then every chunk.
        let sent = Sent::default();
        let mut link = StreamLink::new(Cursor::new(answer), sent.clone());
        let mut got = Vec::new();
        {
            let mut answer = link.call(br#"{"op":"wire_get"}"#, b"").unwrap();
            while let Some(objects) = answer.next_chunk().unwrap() {
                got.extend(objects.into_iter().map(|h| h.payload()[0]));
            }
        }
        assert_eq!(got, [0, 1, 2]);
        assert_eq!(sent.bytes(), b"{\"op\":\"wire_get\"}\n");

        // 3. What it held.
        let one = bytes::to_u64(chunks[0].len());
        assert_eq!(link.peak_bytes(), line_len + one);
    }

    /// A request line past [`MAX_WIRE_LINE`] is refused before a byte is sent; an answer line
    /// past it is refused once that many bytes came, and ends the session.
    #[test]
    fn a_line_past_max_wire_line_is_refused() {
        // 1. The request.
        let sent = Sent::default();
        let mut link = StreamLink::new(Cursor::new(Vec::new()), sent.clone());
        let long = vec![b' '; MAX_WIRE_LINE + 1];
        let refused = link.call(&long, b"").err().unwrap();
        assert_eq!(refused.code, ErrorCode::InvalidArg.as_str());
        assert_eq!(detail(&refused, "reason"), "line_too_long");
        assert_eq!(sent.bytes(), Vec::<u8>::new());

        // 2. The answer, then the next call.
        let answer = vec![b' '; MAX_WIRE_LINE + 2];
        let mut link = StreamLink::new(Cursor::new(answer), Sent::default());
        let refused = link.call(b"{}", b"").err().unwrap();
        assert_eq!(refused.code, ErrorCode::InvalidArg.as_str());
        assert_eq!(detail(&refused, "reason"), "line_too_long");
        let next = link.call(b"{}", b"").err().unwrap();
        assert_eq!(next.code, ErrorCode::RemoteUnreachable.as_str());
        assert_eq!(detail(&next, "cause"), "disconnected");
    }

    /// A peer that sends nothing ends the call at the stall deadline: `E_REMOTE_UNREACHABLE`,
    /// `timeout`, retryable. A link waits [`WIRE_STALL_SECS`] unless a test shortens it.
    #[test]
    fn a_silent_peer_ends_at_the_stall_deadline() {
        let (reader, _writer) = io::pipe().unwrap();
        let link = StreamLink::new(reader, Sent::default());
        assert_eq!(
            link.session.input.stall,
            Duration::from_secs(WIRE_STALL_SECS)
        );

        let stall = Duration::from_millis(100);
        let mut link = link.with_stall(stall);
        let started = Instant::now();
        let refused = link.call(b"{}", b"").err().unwrap();
        assert!(started.elapsed() >= stall);
        assert_eq!(refused.code, ErrorCode::RemoteUnreachable.as_str());
        assert!(refused.retryable);
        assert_eq!(detail(&refused, "cause"), "timeout");
    }

    /// Chunks an answer left unread are skipped by the next call, which reads its own answer.
    #[test]
    fn the_next_call_skips_the_chunks_left_unread() {
        let chunks = chunks(2, 1000);
        let total: usize = chunks.iter().map(Vec::len).sum();
        let mut answers = get_line(2, bytes::to_u64(total));
        answers.extend(chunks.concat());
        let second = get_line(0, 0);
        answers.extend(&second);

        let mut link = StreamLink::new(Cursor::new(answers), Sent::default());
        let mut first = link.call(b"{}", b"").unwrap();
        assert!(first.next_chunk().unwrap().is_some());
        let mut next = link.call(b"{}", b"").unwrap();
        assert_eq!(next.line(), &second[..second.len() - 1]);
        assert!(next.next_chunk().unwrap().is_none());
    }

    /// A peer that closes the stream mid-line or mid-chunk is `disconnected`, retryable.
    #[test]
    fn a_peer_gone_mid_answer_is_disconnected() {
        let chunk = &chunks(1, 1000)[0];
        let mut cut_chunk = get_line(1, bytes::to_u64(chunk.len()));
        cut_chunk.extend(&chunk[..500]);
        let cut_line = br#"{"ok":true"#.to_vec();

        for (answer, at_line) in [(cut_line, true), (cut_chunk, false)] {
            let mut link = StreamLink::new(Cursor::new(answer), Sent::default());
            let refused = match link.call(b"{}", b"") {
                Err(refused) => refused,
                Ok(mut answer) => answer.next_chunk().err().unwrap(),
            };
            assert_eq!(
                refused.code,
                ErrorCode::RemoteUnreachable.as_str(),
                "{at_line}"
            );
            assert!(refused.retryable);
            assert_eq!(detail(&refused, "cause"), "disconnected");
        }
    }
}
