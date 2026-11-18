//! A check's findings (M31-4): its report file, JSON lines it writes at `SWARMOPS_CHECK_REPORT`,
//! read once it has ended; the cuts every finding keeps; the run's bound on what it returns.
//!
//! ```text
//!   bytes (at most MAX_REPORT_BYTES; one past it: cut at the last whole line)
//!     └─ each line, `\n`-ended (a `\r` before it dropped), in file order:
//!          spaces and tabs alone ─▶ blank, counted nowhere
//!          past 64 KiB, not UTF-8, not a JSON object, an unknown key, a value outside its rule
//!                               ─▶ invalid: counted, the first one's number and why kept
//!          else ─▶ a finding: level, title (one line, cut), message (cut), path, line, end_line
//!   Budget: the run's returns, in run order, until 10,000 findings or 4 MiB of their text
//! ```
//!
//! A line is one finding or none: a file cut at its bound loses its last partial line alone.
//! Nothing in a line is followed or opened: a path is a string, held to a repository path.

use serde_json::{Map, Value};
use swarmops_api::{CheckFindingLevel, CheckFindingOut};
use swarmops_core::RepoPath;

use super::{
    MAX_FINDING_LINE_BYTES, MAX_FINDING_MESSAGE_BYTES, MAX_FINDING_PATH_BYTES,
    MAX_FINDING_TITLE_BYTES, MAX_REPORT_BYTES, MAX_RUN_FINDING_BYTES, MAX_RUN_FINDINGS,
};

/// The highest line a finding may name: 2^32.
pub const MAX_FINDING_LINE: u64 = 1 << 32;

/// The keys of a line of the report file, version 1.
const KEYS: [&str; 6] = ["level", "title", "message", "path", "line", "end_line"];

/// What [`read_report_file`] read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct FileRead {
    /// The findings, in line order, each of report 0.
    pub findings: Vec<CheckFindingOut>,
    /// The lines read: findings, invalid lines and blank lines together.
    pub lines: u64,
    /// The lines of spaces and tabs alone, or none, skipped.
    pub blank: u64,
    /// The lines that read as no finding, each skipped.
    pub invalid: u64,
    /// The first of them: its number and why, as `line 3: unknown key "lvl"`.
    pub first_invalid: Option<String>,
    /// Whether the bytes were past the bound: the lines before it were read, the rest not.
    pub cut: bool,
}

/// Reads `bytes` as a report file: at most [`MAX_REPORT_BYTES`] of them, a larger file read to
/// that bound, its last partial line left out. The fuzz target's entry.
pub fn read_report_file(bytes: &[u8]) -> FileRead {
    read_bounded(
        bytes,
        usize::try_from(MAX_REPORT_BYTES).unwrap_or(usize::MAX),
    )
}

/// [`read_report_file`] with a bound of `limit` bytes.
pub fn read_bounded(bytes: &[u8], limit: usize) -> FileRead {
    // 1. The whole lines within the bound.
    let mut read = FileRead::default();
    let mut bytes = bytes;
    if bytes.len() > limit {
        read.cut = true;
        let within = &bytes[..limit];
        let end = within
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |at| at + 1);
        bytes = &within[..end];
    }

    // 2. Each line, in order.
    let mut lines = bytes.split(|&b| b == b'\n').peekable();
    let mut n = 0u64;
    while let Some(line) = lines.next() {
        if line.is_empty() && lines.peek().is_none() {
            break;
        }
        n += 1;
        read.lines += 1;
        match finding_of(line) {
            Line::Blank => read.blank += 1,
            Line::Finding(finding) => read.findings.push(*finding),
            Line::Invalid(why) => {
                read.invalid += 1;
                if read.first_invalid.is_none() {
                    read.first_invalid = Some(format!("line {n}: {why}"));
                }
            }
        }
    }
    read
}

/// What one line of the report file is.
enum Line {
    /// Spaces and tabs alone, or nothing.
    Blank,
    /// A finding.
    Finding(Box<CheckFindingOut>),
    /// No finding, and why, in a few words.
    Invalid(String),
}

/// The line `bytes`, its `\n` gone, as [`Line`].
fn finding_of(bytes: &[u8]) -> Line {
    // 1. Bounded, not blank, text, a JSON object.
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    if bytes.len() > MAX_FINDING_LINE_BYTES {
        return Line::Invalid(format!("longer than {} KiB", MAX_FINDING_LINE_BYTES / 1024));
    }
    if bytes.iter().all(|&b| b == b' ' || b == b'\t') {
        return Line::Blank;
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Line::Invalid("not UTF-8".to_owned());
    };
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return Line::Invalid("not JSON".to_owned());
    };
    let Value::Object(object) = value else {
        return Line::Invalid("not an object".to_owned());
    };
    match fields(&object) {
        Ok(finding) => Line::Finding(Box::new(finding)),
        Err(why) => Line::Invalid(why),
    }
}

/// The finding `object` gives, or why it gives none.
fn fields(object: &Map<String, Value>) -> Result<CheckFindingOut, String> {
    // 1. No key but version 1's.
    if let Some(key) = object.keys().find(|key| !KEYS.contains(&key.as_str())) {
        return Err(format!("unknown key {}", shown(key)));
    }

    // 2. The level and the title, required.
    let level = match object.get("level") {
        None => return Err("missing level".to_owned()),
        Some(Value::String(word)) => CheckFindingLevel::parse(word),
        Some(_) => None,
    };
    let Some(level) = level else {
        return Err("level is not error, warning or notice".to_owned());
    };
    let title = match object.get("title") {
        None => return Err("missing title".to_owned()),
        Some(Value::String(title)) => one_line_title(title),
        Some(_) => return Err("title is not a string".to_owned()),
    };
    let Some(title) = title else {
        return Err("title is empty".to_owned());
    };
    let mut finding = CheckFindingOut::new(level, title);

    // 3. The message, the path and the lines, each under its rule.
    match object.get("message") {
        None => {}
        Some(Value::String(message)) => finding.message = message_of(message),
        Some(_) => return Err("message is not a string".to_owned()),
    }
    match object.get("path") {
        None => {}
        Some(Value::String(path)) if path.len() > MAX_FINDING_PATH_BYTES => {
            return Err(format!(
                "path is longer than {} KiB",
                MAX_FINDING_PATH_BYTES / 1024
            ));
        }
        Some(Value::String(path)) => match RepoPath::new(path.as_str()) {
            Ok(path) => finding.path = Some(path.as_str().to_owned()),
            Err(_) => return Err("path is not a repository path".to_owned()),
        },
        Some(_) => return Err("path is not a string".to_owned()),
    }
    if let Some(line) = object.get("line") {
        let Some(line) = line.as_u64().filter(|n| (1..=MAX_FINDING_LINE).contains(n)) else {
            return Err(format!(
                "line is not a whole number from 1 to {MAX_FINDING_LINE}"
            ));
        };
        if finding.path.is_none() {
            return Err("line without path".to_owned());
        }
        finding.line = Some(line);
    }
    if let Some(end) = object.get("end_line") {
        let Some(line) = finding.line else {
            return Err("end_line without line".to_owned());
        };
        let Some(end) = end
            .as_u64()
            .filter(|n| (line..=MAX_FINDING_LINE).contains(n))
        else {
            return Err(format!(
                "end_line is not a whole number from line to {MAX_FINDING_LINE}"
            ));
        };
        finding.end_line = Some(end);
    }
    Ok(finding)
}

/// `text` as a finding's title: each line break a space, trimmed, cut to
/// [`MAX_FINDING_TITLE_BYTES`]; none when nothing is left.
pub(crate) fn one_line_title(text: &str) -> Option<String> {
    let text = text.replace(['\r', '\n'], " ");
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(cut_end(text, MAX_FINDING_TITLE_BYTES))
}

/// `text` as a finding's message: cut to [`MAX_FINDING_MESSAGE_BYTES`], line breaks kept; none
/// when empty.
pub(crate) fn message_of(text: &str) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    Some(cut_end(text, MAX_FINDING_MESSAGE_BYTES))
}

/// `text`, at most `limit` bytes: when longer, cut at a character's end with `…` after it, the
/// whole within `limit`.
pub(crate) fn cut_end(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit.saturating_sub('…'.len_utf8());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// The most characters of a key that a line's message repeats.
const SHOWN: usize = 32;

/// `key` quoted, its control characters escaped, at most [`SHOWN`] characters of it, `…` after a
/// cut.
fn shown(key: &str) -> String {
    match key.char_indices().nth(SHOWN) {
        Some((at, _)) => format!("{:?}…", &key[..at]),
        None => format!("{key:?}"),
    }
}

/// The run's bound on the findings it returns (D5): at most [`MAX_RUN_FINDINGS`] and
/// [`MAX_RUN_FINDING_BYTES`] of their text (title, group, message and path), in run order. Once
/// a finding would pass either, it and every later one are counted, not returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Budget {
    /// The most findings returned.
    max_findings: usize,
    /// The most bytes of their text returned.
    max_bytes: u64,
    /// The findings returned so far.
    returned: usize,
    /// Their text's bytes.
    bytes: u64,
    /// The findings counted and not returned.
    omitted: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Budget::bounded(MAX_RUN_FINDINGS, MAX_RUN_FINDING_BYTES)
    }
}

impl Budget {
    /// A run's bound: [`MAX_RUN_FINDINGS`] and [`MAX_RUN_FINDING_BYTES`].
    pub fn new() -> Budget {
        Budget::default()
    }

    /// A bound of `max_findings` and `max_bytes`.
    pub fn bounded(max_findings: usize, max_bytes: u64) -> Budget {
        Budget {
            max_findings,
            max_bytes,
            returned: 0,
            bytes: 0,
            omitted: 0,
        }
    }

    /// Whether `finding`, the run's next, is returned; counted as omitted when not.
    pub fn take(&mut self, finding: &CheckFindingOut) -> bool {
        let text = [
            Some(&finding.title),
            finding.group.as_ref(),
            finding.message.as_ref(),
            finding.path.as_ref(),
        ];
        let size: usize = text.iter().flatten().map(|text| text.len()).sum();
        let bytes = self
            .bytes
            .saturating_add(u64::try_from(size).unwrap_or(u64::MAX));
        if self.omitted > 0 || self.returned >= self.max_findings || bytes > self.max_bytes {
            self.omitted += 1;
            return false;
        }
        self.returned += 1;
        self.bytes = bytes;
        true
    }

    /// The findings counted and not returned so far.
    pub fn omitted(&self) -> u64 {
        self.omitted
    }
}

#[cfg(test)]
mod tests;
