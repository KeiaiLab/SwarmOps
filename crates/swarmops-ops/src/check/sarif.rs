//! A SARIF report (M31-5): the JSON log of a scanner's results a check declares, read once it has
//! ended into a finding for each result of kind `fail`, and the count of its results.
//!
//! ```text
//!   bytes ─ no UTF-16 mark ─ UTF-8 ─ nesting of arrays and objects ≤ 64, counted outside strings
//!     ─ JSON, streamed: {version: "2.1.0", runs: [{tool: {driver: {name}}, results: [r, …]}, …]}
//!   each result r, one at a time: counted; of kind fail (or none): a finding
//!     level   error → error, warning → warning, note or none → notice, absent → warning
//!     title   ruleId, else the first line of message.text, else "(no title)"
//!     message message.text            group  its run's tool.driver.name
//!     path    the first location's artifactLocation.uri: a relative reference, percent-decoded,
//!             or a file: URI below the check's directory; joined to the check's dir
//!     line    its region's startLine, end_line its endLine
//! ```
//!
//! Keys the reader does not use are skipped, as the format allows; a value of the wrong type is
//! `shape`, as is a log of another version or without `runs`. `uriBaseId` is not resolved, and a
//! rule's own level is not looked up. A pure function of the bytes: nothing is opened or run.

use std::fmt;

use serde::Deserialize;
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use swarmops_api::{CheckFindingLevel, CheckFindingOut, CheckReportFault};

use super::MAX_REPORT_DEPTH;
use super::findings::{MAX_FINDING_LINE, group_of, message_of, one_line_title};
use super::junit::{Refused, tree_path};

/// The one version of SARIF read.
const SARIF_VERSION: &str = "2.1.0";

/// The title of a result with neither a rule nor a message.
const NO_TITLE: &str = "(no title)";

/// What [`read`] read of a SARIF report.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct SarifRead {
    /// A finding for each result of kind `fail`, in document order: the first of
    /// [`SarifRead::found`], as many as the reader was asked to keep.
    pub findings: Vec<CheckFindingOut>,
    /// The results of kind `fail`, kept or not.
    pub found: u64,
    /// Every result, of every kind.
    pub results: u64,
}

/// Where a report's paths are taken from: the check's directory in the tree, and on the host.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct Place<'a> {
    /// The check's `dir`: `.` or a repository path.
    pub dir: &'a str,
    /// Its absolute path on the host, without a trailing `/`, against which a `file:` URI is
    /// taken; none takes no `file:` URI.
    pub host: Option<&'a str>,
}

impl<'a> Place<'a> {
    /// The check's `dir`, and its absolute path on the host if known.
    pub fn new(dir: &'a str, host: Option<&'a str>) -> Place<'a> {
        Place { dir, host }
    }
}

/// Reads `bytes` as a SARIF report of a check at `place`, keeping every finding. The fuzz
/// target's entry.
pub fn read(bytes: &[u8], place: Place<'_>) -> Result<SarifRead, Refused> {
    read_keeping(bytes, place, usize::MAX)
}

/// [`read`], keeping the first `keep` findings and counting the rest.
pub fn read_keeping(bytes: &[u8], place: Place<'_>, keep: usize) -> Result<SarifRead, Refused> {
    // 1. Text, nested within the bound.
    if bytes.starts_with(&[0xFE, 0xFF]) || bytes.starts_with(&[0xFF, 0xFE]) {
        return Err((
            CheckReportFault::NotText,
            "a UTF-16 byte order mark: only UTF-8 is read".to_owned(),
        ));
    }
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let text = std::str::from_utf8(bytes).map_err(|e| {
        (
            CheckReportFault::NotText,
            format!("not UTF-8, at byte {}", e.valid_up_to()),
        )
    })?;
    if too_deep(bytes) {
        return Err((
            CheckReportFault::TooDeep,
            format!("arrays and objects nested past {MAX_REPORT_DEPTH}"),
        ));
    }

    // 2. The log, its results taken one at a time.
    let mut acc = Acc {
        place,
        keep,
        read: SarifRead::default(),
        version: None,
        runs: false,
    };
    let mut de = serde_json::Deserializer::from_str(text);
    let parsed = LogSeed(&mut acc)
        .deserialize(&mut de)
        .and_then(|()| de.end());
    if let Err(e) = parsed {
        use serde_json::error::Category;
        let at = format!("line {} column {}", e.line(), e.column());
        return Err(match e.classify() {
            Category::Data => (
                CheckReportFault::Shape,
                format!("not a SARIF log: a value of the wrong type, at {at}"),
            ),
            Category::Eof => (
                CheckReportFault::Syntax,
                format!("not JSON: it ends before its last value, at {at}"),
            ),
            Category::Syntax | Category::Io => {
                (CheckReportFault::Syntax, format!("not JSON, at {at}"))
            }
        });
    }

    // 3. Of the format.
    match acc.version.as_deref() {
        Some(SARIF_VERSION) => {}
        Some(other) => {
            return Err((
                CheckReportFault::Shape,
                format!(
                    "version {:?}, not {SARIF_VERSION}",
                    super::findings::cut_end(other, 40)
                ),
            ));
        }
        None => {
            return Err((
                CheckReportFault::Shape,
                format!("no version: a SARIF log names {SARIF_VERSION}"),
            ));
        }
    }
    if !acc.runs {
        return Err((
            CheckReportFault::Shape,
            "no runs: a SARIF log holds an array of runs".to_owned(),
        ));
    }
    Ok(acc.read)
}

/// Whether `bytes` nest arrays and objects past [`MAX_REPORT_DEPTH`], counted outside strings,
/// before any is parsed: the bound is SwarmOps', not the parser's.
fn too_deep(bytes: &[u8]) -> bool {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for &b in bytes {
        if in_string {
            match (escaped, b) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'[' | b'{' => {
                depth += 1;
                if depth > MAX_REPORT_DEPTH {
                    return true;
                }
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    false
}

/// What the walk gathers.
struct Acc<'a> {
    /// Where paths are taken from.
    place: Place<'a>,
    /// The most findings kept.
    keep: usize,
    /// What was read.
    read: SarifRead,
    /// The log's `version`, when it is a string.
    version: Option<String>,
    /// Whether the log holds `runs`.
    runs: bool,
}

/// The log: its `version` and its `runs`; every other key skipped.
struct LogSeed<'a, 'b>(&'b mut Acc<'a>);

impl<'de> DeserializeSeed<'de> for LogSeed<'_, '_> {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<(), D::Error> {
        de.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for LogSeed<'_, '_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a SARIF log")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<(), M::Error> {
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            match key.as_ref() {
                "version" => self.0.version = Some(map.next_value::<String>()?),
                "runs" => {
                    map.next_value_seed(RunsSeed(&mut *self.0))?;
                    self.0.runs = true;
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(())
    }
}

/// The log's `runs`, each in turn.
struct RunsSeed<'a, 'b>(&'b mut Acc<'a>);

impl<'de> DeserializeSeed<'de> for RunsSeed<'_, '_> {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<(), D::Error> {
        de.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for RunsSeed<'_, '_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an array of runs")
    }

    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<(), S::Error> {
        while seq.next_element_seed(RunSeed(&mut *self.0))?.is_some() {}
        Ok(())
    }
}

/// One run: its tool's name, which groups its findings wherever it comes, and its results.
struct RunSeed<'a, 'b>(&'b mut Acc<'a>);

impl<'de> DeserializeSeed<'de> for RunSeed<'_, '_> {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<(), D::Error> {
        de.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for RunSeed<'_, '_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a run")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<(), M::Error> {
        let first = self.0.read.findings.len();
        let mut tool: Option<String> = None;
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            match key.as_ref() {
                "tool" => {
                    let read = map.next_value::<Option<Tool>>()?;
                    tool = read.and_then(|t| t.driver).and_then(|d| d.name);
                }
                "results" => map.next_value_seed(ResultsSeed(&mut *self.0))?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        let group = tool.as_deref().and_then(group_of);
        for finding in &mut self.0.read.findings[first..] {
            finding.group.clone_from(&group);
        }
        Ok(())
    }
}

/// A run's `results`, each read, counted and dropped in turn.
struct ResultsSeed<'a, 'b>(&'b mut Acc<'a>);

impl<'de> DeserializeSeed<'de> for ResultsSeed<'_, '_> {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<(), D::Error> {
        de.deserialize_option(self)
    }
}

impl<'de> Visitor<'de> for ResultsSeed<'_, '_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an array of results")
    }

    fn visit_none<E>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_some<D: serde::Deserializer<'de>>(self, de: D) -> Result<(), D::Error> {
        de.deserialize_seq(self)
    }

    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<(), S::Error> {
        let acc = self.0;
        while let Some(result) = seq.next_element::<SarifResult>()? {
            acc.read.results += 1;
            if result.kind.as_deref().is_some_and(|kind| kind != "fail") {
                continue;
            }
            acc.read.found += 1;
            if acc.read.findings.len() < acc.keep {
                let finding = finding_of(result, acc.place);
                acc.read.findings.push(finding);
            }
        }
        Ok(())
    }
}

/// A run's `tool`.
#[derive(Deserialize)]
struct Tool {
    /// Its driver.
    driver: Option<Driver>,
}

/// A tool's `driver`.
#[derive(Deserialize)]
struct Driver {
    /// Its name: the group of the run's findings.
    name: Option<String>,
}

/// One result: what a finding takes of it.
#[derive(Deserialize)]
struct SarifResult {
    /// Its rule: the finding's title.
    #[serde(rename = "ruleId")]
    rule_id: Option<String>,
    /// Its level.
    level: Option<String>,
    /// Its kind; `fail` when absent.
    kind: Option<String>,
    /// Its message.
    message: Option<Message>,
    /// Its locations: the first gives the path and lines.
    locations: Option<FirstLocation>,
}

/// A result's `locations`, of which only the first is held: the rest are passed over as they
/// are read, so a result holds no more for having many.
struct FirstLocation(Option<Location>);

impl<'de> Deserialize<'de> for FirstLocation {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        /// Reads an array's first location and passes over the rest.
        struct First;

        impl<'de> Visitor<'de> for First {
            type Value = FirstLocation;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an array of locations")
            }

            fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<FirstLocation, S::Error> {
                let first = seq.next_element::<Location>()?;
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(FirstLocation(first))
            }
        }

        de.deserialize_seq(First)
    }
}

/// A result's `message`.
#[derive(Deserialize)]
struct Message {
    /// Its text.
    text: Option<String>,
}

/// A result's location.
#[derive(Deserialize)]
struct Location {
    /// Where in a file.
    #[serde(rename = "physicalLocation")]
    physical: Option<Physical>,
}

/// A location in a file.
#[derive(Deserialize)]
struct Physical {
    /// The file.
    #[serde(rename = "artifactLocation")]
    artifact: Option<Artifact>,
    /// The lines.
    region: Option<Region>,
}

/// A file a location names.
#[derive(Deserialize)]
struct Artifact {
    /// Its URI.
    uri: Option<String>,
}

/// The lines of a location.
#[derive(Deserialize)]
struct Region {
    /// The first.
    #[serde(rename = "startLine")]
    start: Option<u64>,
    /// The last.
    #[serde(rename = "endLine")]
    end: Option<u64>,
}

/// The finding of `result`, of a check at `place`.
fn finding_of(result: SarifResult, place: Place<'_>) -> CheckFindingOut {
    // 1. Its level, title and message.
    let level = match result.level.as_deref() {
        Some("error") => CheckFindingLevel::Error,
        Some("note" | "none") => CheckFindingLevel::Notice,
        _ => CheckFindingLevel::Warning,
    };
    let text = result.message.and_then(|message| message.text);
    let title = (result.rule_id.as_deref())
        .and_then(one_line_title)
        .or_else(|| {
            let first = text
                .as_deref()?
                .lines()
                .find(|line| !line.trim().is_empty())?;
            one_line_title(first)
        })
        .unwrap_or_else(|| NO_TITLE.to_owned());
    let mut finding = CheckFindingOut::new(level, title);
    finding.message = text.as_deref().and_then(message_of);

    // 2. Its path and lines, from the first location.
    let physical = (result.locations)
        .and_then(|locations| locations.0)
        .and_then(|location| location.physical);
    let Some(physical) = physical else {
        return finding;
    };
    let uri = physical.artifact.and_then(|artifact| artifact.uri);
    finding.path = uri.as_deref().and_then(|uri| path_of(uri, place));
    if finding.path.is_some()
        && let Some(region) = physical.region
    {
        finding.line = region
            .start
            .filter(|line| (1..=MAX_FINDING_LINE).contains(line));
        if let Some(line) = finding.line {
            finding.end_line = region
                .end
                .filter(|end| (line..=MAX_FINDING_LINE).contains(end));
        }
    }
    finding
}

/// The repository path `uri` names, of a check at `place`: a relative reference, percent-decoded
/// and joined to the check's `dir`; or a `file:` URI of a path below the check's directory on the
/// host. None for any other URI, a query or fragment, or a path that is not a repository path.
fn path_of(uri: &str, place: Place<'_>) -> Option<String> {
    if uri.contains(['?', '#']) {
        return None;
    }
    let Some((scheme, rest)) = scheme_of(uri) else {
        return tree_path(place.dir, &percent_decoded(uri)?);
    };
    if !scheme.eq_ignore_ascii_case("file") {
        return None;
    }
    let path = match rest.strip_prefix("//") {
        Some(authority) => {
            let (host, path) = authority.split_at(authority.find('/')?);
            if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
                return None;
            }
            path
        }
        None => rest,
    };
    let path = percent_decoded(path)?;
    let below = path.strip_prefix(place.host?)?.strip_prefix('/')?;
    tree_path(place.dir, below)
}

/// The scheme of `uri` and the rest after its `:`, if it has one: a letter, then letters, digits,
/// `+`, `-` and `.`, before any `/`.
fn scheme_of(uri: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = uri.split_once(':')?;
    let mut chars = scheme.chars();
    let first = chars.next()?;
    let grammar = first.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    grammar.then_some((scheme, rest))
}

/// `text` with each `%XX` decoded; none when one is not two hex digits or the bytes are not
/// UTF-8.
fn percent_decoded(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = text.get(at + 1..at + 3)?;
            if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            out.push(u8::from_str_radix(hex, 16).ok()?);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests;
