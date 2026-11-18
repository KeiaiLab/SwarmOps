//! A JUnit report (M31-5): the XML file of test cases a check declares, read once it has ended
//! into a finding for each case that failed or ended in an error, and counts of the cases.
//!
//! ```text
//!   bytes ─ no UTF-16 mark ─ UTF-8 ─ XML, streaming (quick-xml), each event in order:
//!     <?xml encoding?> other than UTF-8 ─▶ not_text      <!DOCTYPE …> ─▶ doctype, at once
//!     &name; other than XML's five ─▶ syntax             nesting past 64 ─▶ too_deep
//!     root other than testsuites or testsuite ─▶ shape   ill-formed, or cut short ─▶ syntax
//!   testcase (any depth): a test; with a failure or error child: failed, one finding
//!     (title its name; group its classname, else the nearest testsuite's name; message the
//!     first failure's message and text, then the end of its system-err; path and line from its
//!     file and line attributes, joined to the check's dir); else with a skipped child: skipped
//! ```
//!
//! Comments and processing instructions are skipped, CDATA is text, and the five predefined
//! entities and character references are decoded; nothing else is expanded, and no document type
//! is read, so no entity can be defined. Every other element and attribute is skipped. The
//! counts are of the cases read, never the suites' own `tests` or `failures` attributes. A pure
//! function of the bytes: nothing is opened or run.

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};
use swarmops_api::{CheckFindingLevel, CheckFindingOut, CheckReportFault};
use swarmops_core::RepoPath;

use super::findings::{MAX_FINDING_LINE, cut_end, cut_front, group_of, one_line_title};
use super::{MAX_FINDING_MESSAGE_BYTES, MAX_FINDING_PATH_BYTES, MAX_REPORT_DEPTH};

/// The title of a case without a name.
const NO_NAME: &str = "(no name)";

/// What [`read`] read of a JUnit report.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct JunitRead {
    /// A finding for each failed case, in document order: the first of [`JunitRead::failed`], as
    /// many as the reader was asked to keep.
    pub findings: Vec<CheckFindingOut>,
    /// The test cases.
    pub tests: u64,
    /// Those that failed or ended in an error.
    pub failed: u64,
    /// Those skipped, and neither failed nor ended in an error.
    pub skipped: u64,
}

/// Why a report is refused, and the words that say so in one line.
pub type Refused = (CheckReportFault, String);

/// Reads `bytes` as a JUnit report of a check that runs in `dir` (`.` or a repository path),
/// keeping every finding. The fuzz target's entry.
pub fn read(bytes: &[u8], dir: &str) -> Result<JunitRead, Refused> {
    read_keeping(bytes, dir, usize::MAX)
}

/// [`read`], keeping the first `keep` findings and counting the rest.
pub fn read_keeping(bytes: &[u8], dir: &str, keep: usize) -> Result<JunitRead, Refused> {
    let text = text_of(bytes)?;
    let mut walk = Walk::new(dir, keep);
    let mut reader = Reader::from_str(text);
    loop {
        let event = reader.read_event().map_err(|_| {
            let at = reader.error_position();
            (
                CheckReportFault::Syntax,
                format!("not well-formed XML, at byte {at}"),
            )
        })?;
        match event {
            Event::Decl(decl) => encoding_of(&decl)?,
            Event::DocType(_) => {
                return Err((
                    CheckReportFault::Doctype,
                    "a document type declaration, which could define entities: not read".to_owned(),
                ));
            }
            Event::Start(element) => walk.open(&element, false)?,
            Event::Empty(element) => walk.open(&element, true)?,
            Event::End(_) => walk.close(),
            Event::Text(text) => walk.text(&text.xml10_content()),
            Event::CData(data) => walk.text(&data.xml10_content()),
            Event::GeneralRef(reference) => {
                let decoded = entity(&reference)?;
                walk.text(&decoded);
            }
            Event::Comment(_) | Event::PI(_) => {}
            Event::Eof => return walk.end(),
        }
    }
}

/// `bytes` as UTF-8 text, a UTF-8 byte order mark left out; `not_text` for a UTF-16 mark or bytes
/// that are not UTF-8.
fn text_of(bytes: &[u8]) -> Result<&str, Refused> {
    if bytes.starts_with(&[0xFE, 0xFF]) || bytes.starts_with(&[0xFF, 0xFE]) {
        return Err((
            CheckReportFault::NotText,
            "a UTF-16 byte order mark: only UTF-8 is read".to_owned(),
        ));
    }
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    std::str::from_utf8(bytes).map_err(|e| {
        (
            CheckReportFault::NotText,
            format!("not UTF-8, at byte {}", e.valid_up_to()),
        )
    })
}

/// Whether the XML declaration `decl` names UTF-8, or no encoding: `not_text` for another.
fn encoding_of(decl: &quick_xml::events::BytesDecl<'_>) -> Result<(), Refused> {
    match decl.encoding() {
        None => Ok(()),
        Some(Ok(name))
            if name.eq_ignore_ascii_case("utf-8") || name.eq_ignore_ascii_case("utf8") =>
        {
            Ok(())
        }
        Some(Ok(name)) => Err((
            CheckReportFault::NotText,
            format!(
                "declares the encoding {:?}: only UTF-8 is read",
                cut_end(&name, 40)
            ),
        )),
        Some(Err(_)) => Err((
            CheckReportFault::Syntax,
            "an XML declaration whose encoding does not read".to_owned(),
        )),
    }
}

/// What the reference `name` (`&name;`) stands for: a character reference, or one of XML's five
/// entities; `syntax` for any other.
fn entity(reference: &quick_xml::events::BytesRef<'_>) -> Result<String, Refused> {
    let bad = || {
        (
            CheckReportFault::Syntax,
            format!(
                "an entity reference &{};, which is not one of XML's five",
                cut_end(reference, 40)
            ),
        )
    };
    if reference.is_char_ref() {
        return match reference.resolve_char_ref() {
            Ok(Some(c)) => Ok(c.to_string()),
            _ => Err((
                CheckReportFault::Syntax,
                format!(
                    "a character reference &{}; that names no character",
                    cut_end(reference, 40)
                ),
            )),
        };
    }
    let text = match &**reference {
        "lt" => "<",
        "gt" => ">",
        "amp" => "&",
        "apos" => "'",
        "quot" => "\"",
        _ => return Err(bad()),
    };
    Ok(text.to_owned())
}

/// The attribute `name` of `element`, decoded (XML's five entities and character references);
/// none without it, `syntax` for one that does not read.
fn attribute(element: &BytesStart<'_>, name: &str) -> Result<Option<String>, Refused> {
    for attr in element.attributes() {
        let attr = attr.map_err(|_| {
            (
                CheckReportFault::Syntax,
                "an attribute that does not read".to_owned(),
            )
        })?;
        if attr.key.as_ref() != name {
            continue;
        }
        let value = attr
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|_| {
                (
                    CheckReportFault::Syntax,
                    format!("the attribute {name} holds an entity that is not one of XML's five"),
                )
            })?;
        return Ok(Some(value.into_owned()));
    }
    Ok(None)
}

/// Checks every attribute of `element` reads, so that an entity in one this reader skips is
/// refused as one in an attribute it reads would be.
fn attributes_read(element: &BytesStart<'_>) -> Result<(), Refused> {
    for attr in element.attributes() {
        let attr = attr.map_err(|_| {
            (
                CheckReportFault::Syntax,
                "an attribute that does not read".to_owned(),
            )
        })?;
        attr.normalized_value(XmlVersion::Implicit1_0)
            .map_err(|_| {
                (
                    CheckReportFault::Syntax,
                    "an attribute holds an entity that is not one of XML's five".to_owned(),
                )
            })?;
    }
    Ok(())
}

/// What is kept of text that a case gathers: more than its finding's message can hold.
const GATHERED_BYTES: usize = 2 * MAX_FINDING_MESSAGE_BYTES;

/// Where the text read now goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sink {
    /// Nowhere.
    Nothing,
    /// The case's first failure or error.
    Failure,
    /// The case's `system-err`.
    Stderr,
}

/// A case being read.
#[derive(Debug, Default)]
struct Case {
    /// The depth of its element.
    depth: usize,
    /// Its `name`.
    name: Option<String>,
    /// Its `classname`, else the nearest enclosing suite's `name`.
    group: Option<String>,
    /// Its `file`.
    file: Option<String>,
    /// Its `line`.
    line: Option<String>,
    /// Whether a `failure` or `error` child was read.
    failed: bool,
    /// Whether a `skipped` child was read.
    skipped: bool,
    /// The first failure's `message`.
    message: Option<String>,
    /// The first failure's text, its start.
    failure: String,
    /// The end of its `system-err` text.
    stderr: String,
}

/// The walk over a report's elements.
struct Walk<'d> {
    /// The check's directory.
    dir: &'d str,
    /// The most findings kept.
    keep: usize,
    /// The depth now: the open elements.
    depth: usize,
    /// Whether the root has been read, and closed.
    root: Option<bool>,
    /// Each open `testsuite`'s depth and `name`.
    suites: Vec<(usize, Option<String>)>,
    /// The open cases, the innermost last.
    cases: Vec<Case>,
    /// Where text goes now, and the depth of the element it is of.
    into: (Sink, usize),
    /// What was read.
    read: JunitRead,
}

impl<'d> Walk<'d> {
    /// A walk of a report of a check in `dir`, keeping `keep` findings.
    fn new(dir: &'d str, keep: usize) -> Walk<'d> {
        Walk {
            dir,
            keep,
            depth: 0,
            root: None,
            suites: Vec::new(),
            cases: Vec::new(),
            into: (Sink::Nothing, 0),
            read: JunitRead::default(),
        }
    }

    /// An element opened (`empty`: and closed at once).
    fn open(&mut self, element: &BytesStart<'_>, empty: bool) -> Result<(), Refused> {
        // 1. Within the depth; the root, once, of the format.
        self.depth += 1;
        if self.depth > MAX_REPORT_DEPTH {
            return Err((
                CheckReportFault::TooDeep,
                format!("elements nested past {MAX_REPORT_DEPTH}"),
            ));
        }
        let name = element.name();
        let name = name.as_ref();
        if self.depth == 1 {
            if self.root.is_some() {
                return Err((CheckReportFault::Syntax, "a second root element".to_owned()));
            }
            if name != "testsuites" && name != "testsuite" {
                return Err((
                    CheckReportFault::Shape,
                    format!(
                        "a root element <{}>, not <testsuites> or <testsuite>",
                        cut_end(name, 40)
                    ),
                ));
            }
            self.root = Some(false);
        }

        // 2. What it is.
        match name {
            "testsuite" => {
                let suite = attribute(element, "name")?;
                self.suites.push((self.depth, suite));
            }
            "testcase" => {
                let group = match attribute(element, "classname")? {
                    Some(class) => Some(class),
                    None => self.suites.iter().rev().find_map(|(_, name)| name.clone()),
                };
                self.cases.push(Case {
                    depth: self.depth,
                    name: attribute(element, "name")?,
                    group,
                    file: attribute(element, "file")?,
                    line: attribute(element, "line")?,
                    ..Case::default()
                });
            }
            "failure" | "error" | "skipped" | "system-err" => self.child(element, name)?,
            _ => attributes_read(element)?,
        }
        if empty {
            self.close();
        }
        Ok(())
    }

    /// A `failure`, `error`, `skipped` or `system-err` element: a case's when it is a child of the
    /// innermost case.
    fn child(&mut self, element: &BytesStart<'_>, name: &str) -> Result<(), Refused> {
        let depth = self.depth;
        let Some(case) = self.cases.last_mut().filter(|case| case.depth + 1 == depth) else {
            return attributes_read(element);
        };
        match name {
            "skipped" => {
                case.skipped = true;
                attributes_read(element)
            }
            "system-err" => {
                self.into = (Sink::Stderr, depth);
                attributes_read(element)
            }
            _ if case.failed => attributes_read(element),
            _ => {
                case.failed = true;
                case.message = attribute(element, "message")?;
                self.into = (Sink::Failure, depth);
                attributes_read(element)
            }
        }
    }

    /// Text read, to where it goes.
    fn text(&mut self, text: &str) {
        let (into, depth) = self.into;
        if into == Sink::Nothing || depth != self.depth {
            return;
        }
        let Some(case) = self.cases.last_mut() else {
            return;
        };
        match into {
            Sink::Failure => {
                if case.failure.len() < GATHERED_BYTES {
                    case.failure.push_str(text);
                }
            }
            Sink::Stderr => {
                case.stderr.push_str(text);
                if case.stderr.len() > 2 * GATHERED_BYTES {
                    let mut start = case.stderr.len() - GATHERED_BYTES;
                    while !case.stderr.is_char_boundary(start) {
                        start += 1;
                    }
                    case.stderr.drain(..start);
                }
            }
            Sink::Nothing => {}
        }
    }

    /// The innermost element closed.
    fn close(&mut self) {
        if self.into.1 == self.depth {
            self.into = (Sink::Nothing, 0);
        }
        if self
            .suites
            .last()
            .is_some_and(|(depth, _)| *depth == self.depth)
        {
            self.suites.pop();
        }
        if self
            .cases
            .last()
            .is_some_and(|case| case.depth == self.depth)
            && let Some(case) = self.cases.pop()
        {
            self.case(&case);
        }
        self.depth = self.depth.saturating_sub(1);
        if self.depth == 0 && self.root.is_some() {
            self.root = Some(true);
        }
    }

    /// A case read whole: counted, and a finding if it failed.
    fn case(&mut self, case: &Case) {
        self.read.tests += 1;
        if !case.failed {
            if case.skipped {
                self.read.skipped += 1;
            }
            return;
        }
        self.read.failed += 1;
        if self.read.findings.len() < self.keep {
            let finding = finding_of(case, self.dir);
            self.read.findings.push(finding);
        }
    }

    /// The end of the document: the counts, or `syntax` for one with no root or cut inside it.
    fn end(self) -> Result<JunitRead, Refused> {
        match self.root {
            Some(true) => Ok(self.read),
            Some(false) => Err((
                CheckReportFault::Syntax,
                "it ends inside an element: cut short".to_owned(),
            )),
            None => Err((
                CheckReportFault::Syntax,
                "no root element: not XML".to_owned(),
            )),
        }
    }
}

/// The finding of the failed `case` of a check in `dir`.
fn finding_of(case: &Case, dir: &str) -> CheckFindingOut {
    // 1. Its title and group.
    let title = (case.name.as_deref())
        .and_then(one_line_title)
        .unwrap_or_else(|| NO_NAME.to_owned());
    let mut finding = CheckFindingOut::new(CheckFindingLevel::Error, title);
    finding.group = case.group.as_deref().and_then(group_of);

    // 2. Its message: the failure's message and text, then the end of its stderr.
    let head: Vec<&str> = [case.message.as_deref(), Some(case.failure.as_str())]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    let mut message = cut_end(&head.join("\n\n"), MAX_FINDING_MESSAGE_BYTES);
    let stderr = case.stderr.trim_end();
    let sep = if message.is_empty() { "" } else { "\n\n" };
    let room = MAX_FINDING_MESSAGE_BYTES.saturating_sub(message.len() + sep.len());
    if !stderr.trim().is_empty() && room > '…'.len_utf8() {
        message.push_str(sep);
        message.push_str(&cut_front(stderr, room));
    }
    if !message.is_empty() {
        finding.message = Some(message);
    }

    // 3. Its path and line, when the case names a file below the check's directory.
    finding.path = case.file.as_deref().and_then(|file| tree_path(dir, file));
    if finding.path.is_some() {
        finding.line = (case.line.as_deref())
            .and_then(|line| line.trim().parse::<u64>().ok())
            .filter(|line| (1..=MAX_FINDING_LINE).contains(line));
    }
    finding
}

/// `file`, a path relative to the check's directory `dir`, as a repository path from the tree's
/// root; none for one that is not a repository path once joined (absolute, `..`) or is past its
/// bound.
pub(crate) fn tree_path(dir: &str, file: &str) -> Option<String> {
    let mut file = file;
    while let Some(rest) = file.strip_prefix("./") {
        file = rest;
    }
    let joined = if dir == super::ROOT_DIR {
        file.to_owned()
    } else {
        format!("{dir}/{file}")
    };
    if joined.len() > MAX_FINDING_PATH_BYTES {
        return None;
    }
    RepoPath::new(joined)
        .ok()
        .map(|path| path.as_str().to_owned())
}

#[cfg(test)]
mod tests;
