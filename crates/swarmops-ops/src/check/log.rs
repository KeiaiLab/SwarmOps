//! A check's log (M31-3): both streams of its output, a line for each line it wrote, each with
//! the time since the check's start, and the runner's own lines around them.
//!
//! ```text
//!     0.004 run start {"check":"unit","run":"0193…","stage":"before-land",…}
//!     0.512 out    Compiling app v0.1.0
//!     0.513 err warning: unused variable
//!    12.040 run section {"n":1,"title":"tests"}
//!    40.877 run end {"outcome":"failed","exit_code":101,"duration_ms":40877,"sections":[…],…}
//!   └─ s.mmm ┘ └kind┘ └ the line's text ─────────────────────────────────────────────────┘
//! ```
//!
//! [`LineLog`] is the [`Output`] the process runner writes a check's output to: it splits each
//! stream into lines, keeps a stream's unfinished line until its `\n` comes, writes a line
//! `swarmops:section <title>` as a `run section` line, and holds the log to its bounds
//! ([`MAX_CHECK_LOG_BYTES`], [`MAX_CHECK_LOG_LINE_BYTES`], [`MAX_CHECK_SECTIONS`]), counting what
//! it leaves out. Each whole line is written as it is read, so a follower reads it at once.
//! [`parse`] reads a log back, a line it does not recognise as `other` (a check can write into
//! its own log).
//!
//! The format is SwarmOps' own and provisional: nothing outside this release reads it.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use camino::Utf8Path;
use serde_json::{Map, Value, json};
use swarmops_api::{CheckOutcome, CheckSectionOut};
pub use swarmops_ws::run::{Output, Stream};

/// The most bytes a check's log holds: 64 MiB. Past it one `run cut` line is written, and later
/// output is counted in `dropped_bytes`, not written; the `end` line is still written.
pub const MAX_CHECK_LOG_BYTES: u64 = 64 << 20;

/// The most bytes of one line's text a log keeps: 16 KiB. The rest of the line is counted in
/// `dropped_bytes`.
pub const MAX_CHECK_LOG_LINE_BYTES: usize = 16 * 1024;

/// The most sections a check opens: past them, a marker line is an ordinary line.
pub const MAX_CHECK_SECTIONS: usize = 1_000;

/// The most bytes of a section's title: a longer one is cut at a character's end, with `…`.
pub const MAX_SECTION_TITLE_BYTES: usize = 200;

/// What a line of a check's output starts with to open a section; the rest, trimmed, is its
/// title.
pub const SECTION_MARKER: &str = "swarmops:section ";

/// What every dir key's hash starts with ([`dir_key`]).
const DIR_KEY_DOMAIN: &[u8] = b"swarmops:check-dir:v0\0";

/// The kind of a log's line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Kind {
    /// A line of the check's stdout.
    Out,
    /// A line of its stderr.
    Err,
    /// The runner's own line: `start`, `section`, `cut` or `end`, and a JSON object.
    Run,
    /// A line not in the format.
    Other,
}

impl Kind {
    /// Its word in the log; `other` for a line not in the format.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Out => "out",
            Kind::Err => "err",
            Kind::Run => "run",
            Kind::Other => "other",
        }
    }
}

/// One line of a log, read back.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Line {
    /// Its number, from 1.
    pub n: u64,
    /// Its time, milliseconds since the check's start; none for [`Kind::Other`].
    pub at_ms: Option<u64>,
    /// Its kind.
    pub kind: Kind,
    /// Its text: after the kind, or the whole line for [`Kind::Other`].
    pub text: String,
}

impl Line {
    /// The runner's line `event`'s object, if this is one: `run <event> {…}`.
    pub fn event(&self, event: &str) -> Option<Map<String, Value>> {
        if self.kind != Kind::Run {
            return None;
        }
        let object = self.text.strip_prefix(event)?.strip_prefix(' ')?;
        match serde_json::from_str(object) {
            Ok(Value::Object(object)) => Some(object),
            _ => None,
        }
    }
}

/// What the `start` line says of a check, besides the time.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Start {
    /// The check's name.
    pub check: String,
    /// The run's id.
    pub run: String,
    /// The stage run.
    pub stage: String,
    /// The tree, in the workspace form.
    pub tree: Option<String>,
    /// The attempt, from 1.
    pub attempt: u32,
    /// The program and its arguments.
    pub argv: Vec<String>,
    /// The directory of the tree it runs in.
    pub dir: String,
    /// Its timeout, in seconds.
    pub timeout_s: u64,
}

impl Start {
    /// The start of check `check` of run `run`, `stage`: no tree, attempt 1, no argv, at the
    /// root, 900 s.
    pub fn new(check: impl Into<String>, run: impl Into<String>, stage: impl Into<String>) -> Self {
        Start {
            check: check.into(),
            run: run.into(),
            stage: stage.into(),
            tree: None,
            attempt: 1,
            argv: Vec::new(),
            dir: ".".to_owned(),
            timeout_s: swarmops_api::DEFAULT_CHECK_TIMEOUT_S,
        }
    }
}

/// How a check ended, as its `end` line says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct End {
    /// Its outcome.
    pub outcome: CheckOutcome,
    /// Its exit code, when it exited.
    pub exit_code: Option<i32>,
    /// The signal that ended it, when one the runner did not send did.
    pub signal: Option<i32>,
}

impl End {
    /// An end as `outcome`, with no exit code or signal.
    pub fn new(outcome: CheckOutcome) -> Self {
        End {
            outcome,
            exit_code: None,
            signal: None,
        }
    }
}

/// What a finished log says: the sections, their times, and the bytes it left out.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Summary {
    /// The check's milliseconds, from its start to the end of its output.
    pub duration_ms: u64,
    /// Its sections, in order.
    pub sections: Vec<CheckSectionOut>,
    /// The bytes of its output not written.
    pub dropped_bytes: u64,
}

/// A section open or closed: its title and when it opened.
#[derive(Debug, Clone)]
struct Section {
    /// Its title.
    title: String,
    /// When it opened, in milliseconds since the start.
    start_ms: u64,
}

/// A stream's line not yet ended: its bytes, at most [`MAX_CHECK_LOG_LINE_BYTES`] of them, and
/// how many more it had.
#[derive(Debug, Default)]
struct Partial {
    /// The bytes kept.
    bytes: Vec<u8>,
    /// The bytes past the line's bound.
    over: u64,
}

/// A check's log being written: the [`Output`] of one check's run.
#[derive(Debug)]
#[non_exhaustive]
pub struct LineLog<W: Write + Send> {
    /// Where the lines go.
    out: W,
    /// The check's start, which every line's time counts from.
    start: Instant,
    /// The most bytes the log holds.
    limit: u64,
    /// The bytes written so far.
    written: u64,
    /// Whether the log reached its bound, its `cut` line written.
    cut: bool,
    /// The output bytes not written.
    dropped: u64,
    /// The sections opened, in order.
    sections: Vec<Section>,
    /// Each stream's unfinished line: stdout's, then stderr's.
    partial: [Partial; 2],
}

impl<W: Write + Send> LineLog<W> {
    /// A log into `out`, its `start` line written now, the check's clock started.
    ///
    /// # Errors
    ///
    /// A write of the `start` line that failed.
    pub fn start(out: W, start: &Start, started_at_ms: u64) -> io::Result<Self> {
        Self::bounded(out, start, started_at_ms, MAX_CHECK_LOG_BYTES)
    }

    /// [`LineLog::start`] with a bound of `limit` bytes in place of [`MAX_CHECK_LOG_BYTES`].
    ///
    /// # Errors
    ///
    /// A write of the `start` line that failed.
    pub fn bounded(out: W, start: &Start, started_at_ms: u64, limit: u64) -> io::Result<Self> {
        let mut log = LineLog {
            out,
            start: Instant::now(),
            limit,
            written: 0,
            cut: false,
            dropped: 0,
            sections: Vec::new(),
            partial: [Partial::default(), Partial::default()],
        };
        let object = json!({
            "check": start.check,
            "run": start.run,
            "stage": start.stage,
            "tree": start.tree,
            "attempt": start.attempt,
            "argv": start.argv,
            "dir": start.dir,
            "timeout_s": start.timeout_s,
            "started_at_ms": started_at_ms,
        });
        log.runner_line("start", &object, Bound::Always)?;
        Ok(log)
    }

    /// The milliseconds since the check's start.
    pub fn elapsed_ms(&self) -> u64 {
        millis(self.start.elapsed())
    }

    /// Ends the log: each stream's unfinished line written, then the `end` line with `end` and
    /// the sections' times. The writer back, and what the log says.
    ///
    /// # Errors
    ///
    /// A write that failed.
    pub fn finish(mut self, end: End) -> io::Result<(W, Summary)> {
        // 1. A last line without its `\n`, of each stream.
        for stream in [Stream::Stdout, Stream::Stderr] {
            let partial = std::mem::take(&mut self.partial[slot(stream)]);
            if !partial.bytes.is_empty() || partial.over > 0 {
                self.line(stream, &partial.bytes, partial.over)?;
            }
        }

        // 2. The sections' times: each to the next one's start, the last to the output's end.
        let duration_ms = self.elapsed_ms();
        let sections = self.closed(duration_ms);

        // 3. The end line.
        let mut object = Map::new();
        object.insert("outcome".to_owned(), end.outcome.as_str().into());
        if let Some(code) = end.exit_code {
            object.insert("exit_code".to_owned(), code.into());
        }
        if let Some(signal) = end.signal {
            object.insert("signal".to_owned(), signal.into());
        }
        object.insert("duration_ms".to_owned(), duration_ms.into());
        let shown: Vec<Value> = (sections.iter().enumerate())
            .map(|(at, section)| {
                json!({"n": at + 1, "title": section.title, "start_ms": section.start_ms,
                       "duration_ms": section.duration_ms})
            })
            .collect();
        object.insert("sections".to_owned(), shown.into());
        object.insert("dropped_bytes".to_owned(), self.dropped.into());
        self.runner_line("end", &Value::Object(object), Bound::Always)?;
        self.out.flush()?;
        let summary = Summary {
            duration_ms,
            sections,
            dropped_bytes: self.dropped,
        };
        Ok((self.out, summary))
    }

    /// The sections, each closed: at the next one's start, the last at `end_ms`.
    fn closed(&self, end_ms: u64) -> Vec<CheckSectionOut> {
        let ends = (self.sections.iter().skip(1).map(|next| next.start_ms)).chain([end_ms]);
        (self.sections.iter().zip(ends))
            .map(|(section, end)| {
                let duration = end.saturating_sub(section.start_ms);
                CheckSectionOut::new(section.title.clone(), section.start_ms, duration)
            })
            .collect()
    }

    /// Writes the line `bytes` of `stream`, `over` more bytes of it left out: a marker as a
    /// `run section` line, any other as an `out` or `err` line.
    fn line(&mut self, stream: Stream, bytes: &[u8], over: u64) -> io::Result<()> {
        // 1. Its text: one `\r` before its end dropped, bytes that are not UTF-8 replaced.
        let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
        let text = String::from_utf8_lossy(bytes);
        self.dropped += over;

        // 2. A marker, while sections may open: a section.
        if let Some(title) = section_title(&text)
            && self.sections.len() < MAX_CHECK_SECTIONS
        {
            let start_ms = self.elapsed_ms();
            let n = self.sections.len() + 1;
            self.sections.push(Section {
                title: title.clone(),
                start_ms,
            });
            let object = json!({"n": n, "title": title});
            let raw = u64::try_from(bytes.len()).unwrap_or(u64::MAX) + 1;
            return self.runner_line("section", &object, Bound::Output(raw));
        }

        // 3. Else the line, under the log's bound.
        let kind = match stream {
            Stream::Stdout => Kind::Out,
            _ => Kind::Err,
        };
        let line = format!("{} {} {text}\n", stamp(self.elapsed_ms()), kind.as_str());
        let raw = u64::try_from(bytes.len()).unwrap_or(u64::MAX) + 1;
        self.write_bounded(line.as_bytes(), Bound::Output(raw))
    }

    /// Writes the runner's line `event` with `object`, under `bound`.
    fn runner_line(&mut self, event: &str, object: &Value, bound: Bound) -> io::Result<()> {
        let line = format!("{} run {event} {object}\n", stamp(self.elapsed_ms()));
        self.write_bounded(line.as_bytes(), bound)
    }

    /// Writes `line` unless the log is past its bound: then a line of output is counted as
    /// dropped (its `raw` bytes), and the first one past it writes the `cut` line. The runner's
    /// `start` and `end` lines are always written.
    fn write_bounded(&mut self, line: &[u8], bound: Bound) -> io::Result<()> {
        let size = u64::try_from(line.len()).unwrap_or(u64::MAX);
        if let Bound::Output(raw) = bound {
            if self.cut {
                self.dropped += raw;
                return Ok(());
            }
            if self.written + size > self.limit {
                self.cut = true;
                self.dropped += raw;
                let cut = format!(
                    "{} run cut {}\n",
                    stamp(self.elapsed_ms()),
                    json!({"limit": self.limit})
                );
                return self.write_raw(cut.as_bytes());
            }
        }
        self.write_raw(line)
    }

    /// Writes `bytes` whole, and counts them.
    fn write_raw(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)?;
        self.written += u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        Ok(())
    }
}

impl<W: Write + Send> Output for LineLog<W> {
    fn write(&mut self, stream: Stream, bytes: &[u8]) -> io::Result<()> {
        let mut rest = bytes;
        while !rest.is_empty() {
            // 1. Up to the next `\n`, onto the stream's unfinished line, within its bound.
            let end = rest.iter().position(|&b| b == b'\n');
            let (piece, after) = match end {
                Some(at) => (&rest[..at], Some(&rest[at + 1..])),
                None => (rest, None),
            };
            let partial = &mut self.partial[slot(stream)];
            let room = MAX_CHECK_LOG_LINE_BYTES.saturating_sub(partial.bytes.len());
            let kept = piece.len().min(room);
            partial.bytes.extend_from_slice(&piece[..kept]);
            partial.over += u64::try_from(piece.len() - kept).unwrap_or(u64::MAX);

            // 2. A whole line: written at once.
            let Some(after) = after else {
                break;
            };
            let partial = std::mem::take(&mut self.partial[slot(stream)]);
            self.line(stream, &partial.bytes, partial.over)?;
            rest = after;
        }
        self.out.flush()
    }
}

/// Whether a line of output is written, and what it counts as dropped past the bound.
#[derive(Debug, Clone, Copy)]
enum Bound {
    /// The runner's `start` or `end`: always written.
    Always,
    /// A line of output (or a marker's section line), of this many bytes of output.
    Output(u64),
}

/// The index of `stream`'s unfinished line.
fn slot(stream: Stream) -> usize {
    match stream {
        Stream::Stdout => 0,
        _ => 1,
    }
}

/// The title of a marker line `text`, if it is one: the rest after [`SECTION_MARKER`], trimmed,
/// not empty, cut to [`MAX_SECTION_TITLE_BYTES`] at a character's end with `…`.
pub fn section_title(text: &str) -> Option<String> {
    let title = text.strip_prefix(SECTION_MARKER)?.trim();
    if title.is_empty() {
        return None;
    }
    if title.len() <= MAX_SECTION_TITLE_BYTES {
        return Some(title.to_owned());
    }
    let mut end = MAX_SECTION_TITLE_BYTES - '…'.len_utf8();
    while !title.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!("{}…", &title[..end]))
}

/// `ms` as a line's time: seconds right-aligned in five places, a dot, three digits.
fn stamp(ms: u64) -> String {
    format!("{:>5}.{:03}", ms / 1_000, ms % 1_000)
}

/// `elapsed` in whole milliseconds.
fn millis(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

/// Parses `text`, a log, into its lines, numbered from 1: each in the format, or else
/// [`Kind::Other`]. A last line without its `\n` is a line too.
pub fn parse(text: &str) -> Vec<Line> {
    (text.lines().enumerate())
        .map(|(at, line)| parse_line(u64::try_from(at).unwrap_or(u64::MAX) + 1, line))
        .collect()
}

/// The line `n`, `line`, parsed.
pub(crate) fn parse_line(n: u64, line: &str) -> Line {
    let other = || Line {
        n,
        at_ms: None,
        kind: Kind::Other,
        text: line.to_owned(),
    };
    // 1. The time: `s.mmm`, after the padding.
    let Some((time, rest)) = line.trim_start_matches(' ').split_once(' ') else {
        return other();
    };
    let Some((seconds, millis)) = time.split_once('.') else {
        return other();
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(seconds) || millis.len() != 3 || !digits(millis) {
        return other();
    }
    let (Ok(seconds), Ok(millis)) = (seconds.parse::<u64>(), millis.parse::<u64>()) else {
        return other();
    };

    // 2. The kind, and the text after it.
    let Some((kind, text)) = rest.split_once(' ') else {
        return other();
    };
    let kind = match kind {
        "out" => Kind::Out,
        "err" => Kind::Err,
        "run" => Kind::Run,
        _ => return other(),
    };
    Line {
        n,
        at_ms: Some(seconds.saturating_mul(1_000).saturating_add(millis)),
        kind,
        text: text.to_owned(),
    }
}

/// The key of a directory's runs: the first 32 hex digits of the BLAKE3 hash of its canonical
/// absolute path, after a domain of its own. Two spellings of one directory give one key.
pub fn dir_key(canonical: &Utf8Path) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(DIR_KEY_DOMAIN);
    hasher.update(canonical.as_str().as_bytes());
    let mut key = hasher.finalize().to_hex().to_string();
    key.truncate(32);
    key
}

#[cfg(test)]
mod tests;
