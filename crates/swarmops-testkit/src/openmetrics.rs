//! A test-only reader of the OpenMetrics text format (the M21 design §10.2), written in this
//! repository from the format's rules, so that a test checks the text `health` prints without a
//! third-party parser.
//!
//! ```text
//!   # TYPE <family> gauge      one per family, before its samples
//!   <family>{k="v",...} <int>  a family's samples contiguous; no timestamp
//!   ...
//!   # EOF                      the last line
//! ```
//!
//! Every line it cannot place is an error; [`parse`] returns them all.

use std::collections::{BTreeMap, BTreeSet};

/// The text's last line.
const EOF: &str = "# EOF";

/// A family's type line, before its name and type.
const TYPE: &str = "# TYPE ";

/// The comment lines the format has besides `# TYPE`.
const OTHER_COMMENTS: [&str; 2] = ["# HELP ", "# UNIT "];

/// The types a family may have, and the suffixes their samples take.
const TYPES: [(&str, &[&str]); 3] = [
    ("gauge", &[""]),
    ("unknown", &[""]),
    ("counter", &["_total", "_created"]),
];

/// One sample: its name, its labels in their order, its integer value.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Sample {
    /// The sample's metric name.
    pub name: String,
    /// Its labels, as written.
    pub labels: Vec<(String, String)>,
    /// Its value.
    pub value: i64,
}

/// The samples of `text`, or every rule it breaks, each error naming its line.
pub fn parse(text: &str) -> Result<Vec<Sample>, Vec<String>> {
    let mut reader = Reader::default();

    // 1. The text ends in `# EOF` and one newline; nothing follows.
    let Some(body) = text.strip_suffix('\n') else {
        return Err(vec![
            "the text does not end in # EOF and a newline".to_owned(),
        ]);
    };
    let lines: Vec<&str> = body.split('\n').collect();
    let eof = lines.iter().position(|l| *l == EOF);
    match eof {
        None => reader.error(lines.len(), "no # EOF line"),
        Some(at) if at + 1 != lines.len() => reader.error(at + 2, "a line after # EOF"),
        Some(_) => {}
    }

    // 2. Each line before it, in order.
    let end = eof.unwrap_or(lines.len());
    for (index, line) in lines.iter().take(end).enumerate() {
        reader.line(index + 1, line);
    }
    if reader.errors.is_empty() {
        return Ok(reader.samples);
    }
    Err(reader.errors)
}

/// What the lines read so far declared.
#[derive(Default)]
struct Reader {
    /// Each family's type.
    types: BTreeMap<String, &'static [&'static str]>,
    /// The family whose samples are being read, and the families before it.
    current: Option<String>,
    closed: BTreeSet<String>,
    /// Each sample's name and sorted labels.
    seen: BTreeSet<(String, Vec<(String, String)>)>,
    samples: Vec<Sample>,
    errors: Vec<String>,
}

impl Reader {
    /// Notes the error `what` at line `number`.
    fn error(&mut self, number: usize, what: &str) {
        self.errors.push(format!("line {number}: {what}"));
    }

    /// Places one line: a comment or a sample.
    fn line(&mut self, number: usize, line: &str) {
        if line.is_empty() {
            return self.error(number, "a blank line");
        }
        if let Some(rest) = line.strip_prefix(TYPE) {
            return self.type_line(number, rest);
        }
        if OTHER_COMMENTS.iter().any(|c| line.starts_with(c)) {
            return;
        }
        if line.starts_with('#') {
            return self.error(number, "a comment the format does not have");
        }
        match sample(line) {
            Ok(sample) => self.sample(number, sample),
            Err(what) => self.error(number, &what),
        }
    }

    /// `# TYPE <family> <type>`: once per family, before its samples.
    fn type_line(&mut self, number: usize, rest: &str) {
        // 1. A name and a known type.
        let Some((family, kind)) = rest.split_once(' ') else {
            return self.error(number, "a # TYPE line without a type");
        };
        if !metric_name(family) {
            return self.error(number, "a bad metric name");
        }
        let Some((_, suffixes)) = TYPES.iter().find(|(name, _)| *name == kind) else {
            return self.error(number, "a type the reader does not place");
        };

        // 2. One per family, and the family starts here.
        if self.types.contains_key(family) {
            return self.error(number, "a second # TYPE for one family");
        }
        self.types.insert(family.to_owned(), suffixes);
        self.enter(number, family);
    }

    /// One sample: under its family's `# TYPE`, its family contiguous, never twice.
    fn sample(&mut self, number: usize, sample: Sample) {
        // 1. Its family: a declared one whose type gives the sample's name.
        let family = self.types.iter().find_map(|(family, suffixes)| {
            let rest = sample.name.strip_prefix(family.as_str())?;
            suffixes.contains(&rest).then(|| family.clone())
        });
        let Some(family) = family else {
            return self.error(number, "a sample before its # TYPE");
        };
        self.enter(number, &family);

        // 2. Each label once; the name and label set once in the text.
        let mut keys: Vec<(String, String)> = sample.labels.clone();
        keys.sort();
        if keys.windows(2).any(|w| w[0].0 == w[1].0) {
            return self.error(number, "a label repeated in one sample");
        }
        if !self.seen.insert((sample.name.clone(), keys)) {
            return self.error(number, "a duplicate sample");
        }
        self.samples.push(sample);
    }

    /// Makes `family` the current one, refusing a family that was left before.
    fn enter(&mut self, number: usize, family: &str) {
        if self.current.as_deref() == Some(family) {
            return;
        }
        if self.closed.contains(family) {
            return self.error(number, "a split family: its lines are not contiguous");
        }
        if let Some(left) = self.current.replace(family.to_owned()) {
            self.closed.insert(left);
        }
    }
}

/// One sample line: a name, optional labels in braces, one space, a decimal integer.
fn sample(line: &str) -> Result<Sample, String> {
    // 1. The name, up to the braces or the space.
    let end = line.find(['{', ' ']).ok_or("a sample without a value")?;
    let (name, mut rest) = line.split_at(end);
    if !metric_name(name) {
        return Err("a bad metric name".to_owned());
    }

    // 2. The labels, when braces follow.
    let mut labels = Vec::new();
    if let Some(inner) = rest.strip_prefix('{') {
        rest = read_labels(inner, &mut labels)?;
    }

    // 3. One space and an integer, nothing after.
    let value = rest.strip_prefix(' ').ok_or("no space before the value")?;
    let digits = value.strip_prefix('-').unwrap_or(value);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err("a value that is not a decimal integer".to_owned());
    }
    let value = value
        .parse::<i64>()
        .map_err(|_| "a decimal integer out of range")?;
    Ok(Sample {
        name: name.to_owned(),
        labels,
        value,
    })
}

/// Reads `k="v"` pairs separated by commas up to the closing brace; the text after it.
fn read_labels<'t>(
    mut rest: &'t str,
    labels: &mut Vec<(String, String)>,
) -> Result<&'t str, String> {
    if let Some(after) = rest.strip_prefix('}') {
        return Ok(after);
    }
    loop {
        // 1. A label name and `="`.
        let (key, after) = rest.split_once("=\"").ok_or("a label without =\"")?;
        if !label_name(key) {
            return Err("a bad label name".to_owned());
        }

        // 2. The value, unescaped up to its closing quote.
        let (value, after) = label_value(after)?;
        labels.push((key.to_owned(), value));

        // 3. A comma and the next label, or the closing brace.
        if let Some(after) = after.strip_prefix('}') {
            return Ok(after);
        }
        rest = after
            .strip_prefix(',')
            .ok_or("a label not followed by , or }")?;
    }
}

/// A label value up to its closing quote, with `\\`, `\"` and `\n` unescaped; the text after
/// the quote.
fn label_value(text: &str) -> Result<(String, &str), String> {
    let mut value = String::new();
    let mut chars = text.char_indices();
    while let Some((at, c)) = chars.next() {
        match c {
            '"' => return Ok((value, &text[at + 1..])),
            '\\' => match chars.next().map(|(_, e)| e) {
                Some('\\') => value.push('\\'),
                Some('"') => value.push('"'),
                Some('n') => value.push('\n'),
                _ => return Err("a label value with a bad escape".to_owned()),
            },
            c => value.push(c),
        }
    }
    Err("a label value without its closing quote".to_owned())
}

/// Whether `name` is a metric name: `[a-zA-Z_:][a-zA-Z0-9_:]*`.
fn metric_name(name: &str) -> bool {
    let first = |b: u8| b.is_ascii_alphabetic() || b == b'_' || b == b':';
    let rest = |b: u8| first(b) || b.is_ascii_digit();
    valid(name, first, rest)
}

/// Whether `name` is a label name: `[a-zA-Z_][a-zA-Z0-9_]*`.
fn label_name(name: &str) -> bool {
    let first = |b: u8| b.is_ascii_alphabetic() || b == b'_';
    let rest = |b: u8| first(b) || b.is_ascii_digit();
    valid(name, first, rest)
}

/// Whether `name` is not empty, its first byte passes `first` and every other `rest`.
fn valid(name: &str, first: impl Fn(u8) -> bool, rest: impl Fn(u8) -> bool) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(first) && bytes.all(rest)
}

#[cfg(test)]
mod tests {
    use super::{Sample, parse};

    /// Two families, one with labels, ending in `# EOF`.
    const GOOD: &str = "\
# TYPE swarmops_alert gauge
swarmops_alert{rule=\"busy\"} 0
swarmops_alert{rule=\"land_p99\"} 1
# TYPE swarmops_health_factor_milli gauge
swarmops_health_factor_milli 2000
# EOF
";

    /// Whether `text` is refused with an error that names `what`.
    fn refused(text: &str, what: &str) {
        let errors = parse(text).expect_err("a planted violation is refused");
        let named = errors.iter().any(|e| e.contains(what));
        assert!(named, "no error names {what:?}: {errors:?}");
    }

    /// The format's text reads as its samples, in order, labels in order.
    #[test]
    fn the_parser_accepts_the_format() {
        let samples = parse(GOOD).unwrap();
        let first = Sample {
            name: "swarmops_alert".to_owned(),
            labels: vec![("rule".to_owned(), "busy".to_owned())],
            value: 0,
        };
        assert_eq!(samples.len(), 3, "{samples:?}");
        assert_eq!(samples[0], first);
        assert_eq!(samples[2].labels, Vec::new());
        assert_eq!(samples[2].value, 2_000);

        // An escaped label value reads back unescaped.
        let escaped = "# TYPE a gauge\na{l=\"x\\\\y\\\"z\\n\"} -3\n# EOF\n";
        let samples = parse(escaped).unwrap();
        assert_eq!(samples[0].labels[0].1, "x\\y\"z\n");
        assert_eq!(samples[0].value, -3);
    }

    /// A sample before its family's `# TYPE`, or with none, is refused.
    #[test]
    fn a_sample_before_its_type_is_refused() {
        refused("a 1\n# TYPE a gauge\n# EOF\n", "before its # TYPE");
        refused("a 1\n# EOF\n", "before its # TYPE");
    }

    /// A family whose samples are not contiguous is refused.
    #[test]
    fn a_split_family_is_refused() {
        let text = "# TYPE a gauge\na{x=\"1\"} 1\n# TYPE b gauge\nb 1\na{x=\"2\"} 2\n# EOF\n";
        refused(text, "split");
    }

    /// Anything after `# EOF` is refused.
    #[test]
    fn text_after_eof_is_refused() {
        refused("# TYPE a gauge\na 1\n# EOF\na 2\n", "after # EOF");
        refused("# TYPE a gauge\na 1\n# EOF\n\n", "after # EOF");
    }

    /// A label value with an escape the format does not have, or unterminated, is refused.
    #[test]
    fn a_bad_label_escape_is_refused() {
        refused("# TYPE a gauge\na{x=\"\\t\"} 1\n# EOF\n", "escape");
        refused("# TYPE a gauge\na{x=\"open} 1\n# EOF\n", "label");
    }

    /// A text that does not end in `# EOF` and a newline is refused.
    #[test]
    fn a_missing_eof_is_refused() {
        refused("# TYPE a gauge\na 1\n", "# EOF");
        refused("# TYPE a gauge\na 1\n# EOF", "# EOF");
    }

    /// A metric name or a label name outside the format's characters is refused.
    #[test]
    fn a_bad_metric_name_is_refused() {
        refused("# TYPE 1a gauge\n1a 1\n# EOF\n", "metric name");
        refused("# TYPE a gauge\na{1x=\"1\"} 1\n# EOF\n", "label name");
    }

    /// A label named twice in one sample is refused.
    #[test]
    fn a_repeated_label_is_refused() {
        refused("# TYPE a gauge\na{x=\"1\",x=\"2\"} 1\n# EOF\n", "repeated");
    }

    /// Two samples of one name and one label set are refused, whatever the labels' order.
    #[test]
    fn a_duplicate_sample_is_refused() {
        refused("# TYPE a gauge\na 1\na 2\n# EOF\n", "duplicate");
        let text = "# TYPE a gauge\na{x=\"1\",y=\"2\"} 1\na{y=\"2\",x=\"1\"} 2\n# EOF\n";
        refused(text, "duplicate");
    }

    /// A second `# TYPE` line for one family is refused.
    #[test]
    fn a_second_type_line_is_refused() {
        refused(
            "# TYPE a gauge\n# TYPE a gauge\na 1\n# EOF\n",
            "second # TYPE",
        );
    }

    /// A value that is no decimal integer, a timestamp, or a line not of the sample's shape is
    /// refused.
    #[test]
    fn a_non_number_value_is_refused() {
        refused("# TYPE a gauge\na 1.5\n# EOF\n", "integer");
        refused("# TYPE a gauge\na NaN\n# EOF\n", "integer");
        refused("# TYPE a gauge\na 1 1700000000\n# EOF\n", "integer");
        refused("# TYPE a gauge\na  1\n# EOF\n", "integer");
    }

    /// A gauge's sample named other than its family, and a blank line, are refused.
    #[test]
    fn a_line_out_of_place_is_refused() {
        refused("# TYPE a gauge\na_total 1\n# EOF\n", "before its # TYPE");
        refused("# TYPE a gauge\n\na 1\n# EOF\n", "blank");
        refused("# TYPE a gauge\n# a comment\na 1\n# EOF\n", "comment");
    }
}
