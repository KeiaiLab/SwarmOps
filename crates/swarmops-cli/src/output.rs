//! Printing an envelope (operations spec §0): JSON, compact or pretty, or a short text
//! rendering for `--human`.
//!
//! JSON is written from the [`Envelope`] struct itself, never through `serde_json::Value`
//! (which sorts keys), so the fields keep the spec's order: `ok, op, api, result | error,
//! meta`. One document per command, then a newline.
//!
//! The text rendering prints the result as `key: value` lines in the same order, a nested key
//! as `outer.inner` and an array item as `key[i]`; a multi-line string as `key: |` and its
//! lines indented by two spaces; control characters other than line breaks escaped. A failure
//! prints `error: <code>: <message>`, then its details, `retryable: true` when it is, and the
//! hint. The exit code is the same in both formats.

use std::fmt;
use std::fmt::Write as _;

use serde::Serialize;
use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use swarmops_api::Envelope;

/// How a command prints its envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// The envelope as JSON, indented when `pretty`.
    Json {
        /// Indent the document over several lines.
        pretty: bool,
    },
    /// A short text rendering, not JSON.
    Human,
}

/// What a command prints on stdout, and the code it exits with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Printed {
    /// The text for stdout, ending in a newline.
    pub stdout: String,
    /// The exit code: 0 for success, the error's otherwise.
    pub exit: u8,
}

/// `envelope` printed in `format`, and its exit code.
pub fn render<T: Serialize>(envelope: &Envelope<T>, format: Format) -> Printed {
    let stdout = match format {
        Format::Json { pretty } => json(envelope, pretty),
        Format::Human => human(envelope),
    };
    Printed {
        stdout,
        exit: envelope
            .error
            .as_ref()
            .map_or(0, |error| exit_code(error.exit)),
    }
}

/// The process exit code of an error whose envelope says `exit`: that, unless it is not one
/// a failure can exit with (0, or outside 1..=255), which is 1.
fn exit_code(exit: i32) -> u8 {
    u8::try_from(exit)
        .ok()
        .filter(|&code| code != 0)
        .unwrap_or(1)
}

fn json<T: Serialize>(envelope: &Envelope<T>, pretty: bool) -> String {
    let text = if pretty {
        serde_json::to_string_pretty(envelope)
    } else {
        serde_json::to_string(envelope)
    };
    // Every API type serializes: their maps have string keys, their impls never fail.
    let mut text = text.expect("an envelope serializes");
    text.push('\n');
    text
}

fn human<T: Serialize>(envelope: &Envelope<T>) -> String {
    let mut out = String::new();
    if let Some(result) = &envelope.result {
        lines(&mut out, "", &Tree::of(result));
    }
    if let Some(error) = &envelope.error {
        line(
            &mut out,
            "error",
            &format!("{}: {}", printable(&error.code), printable(&error.message)),
        );
        for (key, value) in &error.details {
            lines(&mut out, &format!("details.{key}"), &Tree::of(value));
        }
        if error.retryable {
            line(&mut out, "retryable", "true");
        }
        if let Some(hint) = &error.hint {
            lines(&mut out, "hint", &Tree::String(hint.clone()));
        }
    }
    out
}

/// `tree`, under `key` (`""` at the top), as `key: value` lines.
fn lines(out: &mut String, key: &str, tree: &Tree) {
    match tree {
        Tree::Object(members) if !members.is_empty() => {
            for (name, value) in members {
                let key = if key.is_empty() {
                    name.clone()
                } else {
                    format!("{key}.{name}")
                };
                lines(out, &key, value);
            }
        }
        Tree::Array(items) if !items.is_empty() => {
            for (i, item) in items.iter().enumerate() {
                lines(out, &format!("{key}[{i}]"), item);
            }
        }
        Tree::String(text) if text.contains('\n') => {
            line(out, key, "|");
            for text_line in text.lines() {
                let _ = writeln!(out, "  {}", printable(text_line));
            }
        }
        Tree::String(text) if text.is_empty() => line(out, key, "\"\""),
        Tree::String(text) => line(out, key, &printable(text)),
        Tree::Object(_) => line(out, key, "{}"),
        Tree::Array(_) => line(out, key, "[]"),
        Tree::Null => line(out, key, "null"),
        Tree::Bool(b) => line(out, key, &b.to_string()),
        Tree::Number(n) => line(out, key, n),
    }
}

/// One `key: value` line; just the value under the empty key.
fn line(out: &mut String, key: &str, value: &str) {
    if key.is_empty() {
        let _ = writeln!(out, "{value}");
    } else {
        let _ = writeln!(out, "{}: {value}", printable(key));
    }
}

/// `text` with each control character escaped (`\n`, `\u{1b}`…), so that what a file or a
/// name holds cannot drive the terminal or break a line.
pub fn printable(text: &str) -> String {
    text.chars()
        .fold(String::with_capacity(text.len()), |mut out, c| {
            if c.is_control() {
                out.extend(c.escape_default());
            } else {
                out.push(c);
            }
            out
        })
}

/// A JSON value that keeps object keys in the order they were written, which
/// `serde_json::Value` does not; numbers are kept as their JSON text.
#[derive(Debug, Clone, PartialEq)]
enum Tree {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Tree>),
    Object(Vec<(String, Tree)>),
}

impl Tree {
    /// `value` as JSON, in the order it serializes its fields.
    fn of<T: Serialize + ?Sized>(value: &T) -> Tree {
        let text = serde_json::to_string(value).expect("an API value serializes");
        serde_json::from_str(&text).expect("serde_json reads back what it wrote")
    }
}

impl<'de> Deserialize<'de> for Tree {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct TreeVisitor;

        impl<'de> Visitor<'de> for TreeVisitor {
            type Value = Tree;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON value")
            }

            fn visit_unit<E>(self) -> Result<Tree, E> {
                Ok(Tree::Null)
            }

            fn visit_bool<E>(self, b: bool) -> Result<Tree, E> {
                Ok(Tree::Bool(b))
            }

            fn visit_i64<E>(self, n: i64) -> Result<Tree, E> {
                Ok(Tree::Number(n.to_string()))
            }

            fn visit_u64<E>(self, n: u64) -> Result<Tree, E> {
                Ok(Tree::Number(n.to_string()))
            }

            fn visit_f64<E>(self, n: f64) -> Result<Tree, E> {
                Ok(Tree::Number(n.to_string()))
            }

            fn visit_str<E>(self, s: &str) -> Result<Tree, E> {
                Ok(Tree::String(s.to_owned()))
            }

            fn visit_string<E>(self, s: String) -> Result<Tree, E> {
                Ok(Tree::String(s))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Tree, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Tree::Array(items))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Tree, A::Error> {
                let mut members = Vec::new();
                while let Some(member) = map.next_entry()? {
                    members.push(member);
                }
                Ok(Tree::Object(members))
            }
        }

        deserializer.deserialize_any(TreeVisitor)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::{Map, Value, json};
    use swarmops_api::{ErrorCode, Meta, OpError};

    use super::*;

    fn meta() -> Meta {
        Meta {
            repo: Some("/s".to_owned()),
            elapsed_ms: 3,
            resolved: BTreeMap::from([("main".to_owned(), "ab".repeat(32))]),
            auth: "trust".to_owned(),
        }
    }

    #[test]
    fn json_keeps_the_spec_field_order() {
        let ok = Envelope::ok("show", json!({"z": 1, "a": 2}), meta());
        let compact = render(&ok, Format::Json { pretty: false });
        assert_eq!(compact.exit, 0);
        assert!(
            compact
                .stdout
                .starts_with(r#"{"ok":true,"op":"show","api":1,"result":{"a":2,"z":1},"meta":{"#),
            "{}",
            compact.stdout
        );
        assert!(compact.stdout.ends_with("\"auth\":\"trust\"}}\n"));
        let pretty = render(&ok, Format::Json { pretty: true });
        assert!(
            pretty
                .stdout
                .starts_with("{\n  \"ok\": true,\n  \"op\": \"show\",\n")
        );
        assert_eq!(
            serde_json::from_str::<Value>(&pretty.stdout).unwrap(),
            serde_json::from_str::<Value>(&compact.stdout).unwrap()
        );

        let failed: Envelope<()> = Envelope::err(
            "show",
            OpError::new(ErrorCode::CasMismatch, "moved"),
            meta(),
        );
        let printed = render(&failed, Format::Json { pretty: false });
        assert_eq!(printed.exit, 4);
        assert!(printed.stdout.starts_with(
            r#"{"ok":false,"op":"show","api":1,"error":{"code":"E_CAS_MISMATCH","message":"moved","retryable":true,"exit":4,"details":{},"hint":null},"meta":"#
        ));
    }

    #[test]
    fn an_error_always_exits_nonzero() {
        assert_eq!(exit_code(6), 6);
        assert_eq!(exit_code(0), 1);
        assert_eq!(exit_code(-1), 1);
        assert_eq!(exit_code(256), 1);
    }

    #[test]
    fn human_text_flattens_the_result_in_field_order() {
        #[derive(Serialize)]
        struct Out {
            name: &'static str,
            nested: Value,
            list: Vec<u8>,
            empty: Vec<u8>,
            none: Option<u8>,
            text: &'static str,
            blank: &'static str,
            odd: &'static str,
        }
        let out = Out {
            name: "acme",
            nested: json!({"b": true, "a": {"deep": 1.5}}),
            list: vec![7, 8],
            empty: vec![],
            none: None,
            text: "line one\r\nline two\n",
            blank: "",
            odd: "bell\u{7}esc\u{1b}[31m",
        };
        let printed = render(&Envelope::ok("x", out, meta()), Format::Human);
        assert_eq!(printed.exit, 0);
        assert_eq!(
            printed.stdout,
            "name: acme\n\
             nested.a.deep: 1.5\n\
             nested.b: true\n\
             list[0]: 7\n\
             list[1]: 8\n\
             empty: []\n\
             none: null\n\
             text: |\n  line one\n  line two\n\
             blank: \"\"\n\
             odd: bell\\u{7}esc\\u{1b}[31m\n"
        );
    }

    #[test]
    fn human_text_of_a_failure() {
        let mut details = Map::new();
        details.insert("name".to_owned(), Value::from("main"));
        details.insert("actual".to_owned(), Value::Null);
        let error = OpError::new(ErrorCode::CasMismatch, "ref main moved")
            .with_details(details)
            .with_hint("swarmops ref list");
        let failed: Envelope<()> = Envelope::err("ref_set", error, meta());
        let printed = render(&failed, Format::Human);
        assert_eq!(printed.exit, 4);
        assert_eq!(
            printed.stdout,
            "error: E_CAS_MISMATCH: ref main moved\n\
             details.actual: null\n\
             details.name: main\n\
             retryable: true\n\
             hint: swarmops ref list\n"
        );
        let usage: Envelope<()> = Envelope::err(
            "log",
            OpError::new(ErrorCode::InvalidArg, "bad").with_hint("Usage: x\nmore"),
            meta(),
        );
        assert_eq!(
            render(&usage, Format::Human).stdout,
            "error: E_INVALID_ARG: bad\nhint: |\n  Usage: x\n  more\n"
        );
    }
}
