//! git's text shapes for `--human` (the git-familiar command design, §3 "Text shapes"): the
//! operations a git user reads most print as git prints them, `ws status` as `git status -sb`,
//! `log` as `git log --oneline`. Only the text form changes; the JSON envelope is untouched.
//!
//! ```text
//! op + result ─▶ text ─▶ Some(git's shape)
//!                   └──▶ None: another op, or a field missing ─▶ the generic key: value lines
//! ```

use std::fmt::Write as _;

use serde_json::Value;

use crate::output::printable;

/// How many hex digits of an id a line shows: the shortest prefix a revision takes.
const SHORT_ID: usize = 12;

/// The last line of a result that holds more than it printed.
const TRUNCATED: &str = "(truncated)\n";

/// Gives `op`'s `result` in git's shape; `None` for another operation, or for a result that lacks
/// a field the shape reads.
pub(crate) fn text(op: &str, result: &Value) -> Option<String> {
    match op {
        "ws_status" => status(result),
        "log" => log(result),
        _ => None,
    }
}

/// `ws status` as `git status -sb`: `## <name> <head12>[ behind main by <n>][ rebasing]`, then
/// `<L>  <path>` a change, or `nothing to commit, workspace clean`.
fn status(result: &Value) -> Option<String> {
    let name = string(result, "name")?;
    let head = string(result, "head")?;
    let behind = result.get("behind_by")?.as_u64()?;
    let state = string(result, "state")?;
    let changes = result.get("changes")?.as_array()?;
    let truncated = result.get("truncated")?.as_bool()?;

    // 1. The header: the workspace, its head, and what it waits for.
    let mut out = format!("## {} {}", printable(name), short(head));
    if behind != 0 {
        let _ = write!(out, " behind main by {behind}");
    }
    if state == "rebasing" {
        out.push_str(" rebasing");
    }
    out.push('\n');

    // 2. One line a change, its letter and two spaces, as git's short form.
    for change in changes {
        let path = string(change, "path")?;
        let kind = string(change, "kind")?;
        let _ = writeln!(out, "{}  {}", letter(kind), printable(path));
    }
    if changes.is_empty() {
        out.push_str("nothing to commit, workspace clean\n");
    }

    if truncated {
        out.push_str(TRUNCATED);
    }
    Some(out)
}

/// `log` as `git log --oneline`: `<id12> <title>` a changeset, newest first.
fn log(result: &Value) -> Option<String> {
    let entries = result.get("entries")?.as_array()?;
    let truncated = result.get("truncated")?.as_bool()?;

    let mut out = String::new();
    for entry in entries {
        let id = string(entry, "id")?;
        let title = string(entry, "title")?;
        let _ = writeln!(out, "{} {}", short(id), printable(title));
    }

    if truncated {
        out.push_str(TRUNCATED);
    }
    Some(out)
}

/// git's letter for a change `kind` (a workspace change or a diff's file status): `?` for a kind
/// this build does not know.
fn letter(kind: &str) -> char {
    match kind {
        "added" => 'A',
        "modified" | "mode_changed" => 'M',
        "deleted" => 'D',
        "type_changed" => 'T',
        _ => '?',
    }
}

/// The string member `key` of `value`.
fn string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key)?.as_str()
}

/// `id` cut to [`SHORT_ID`] characters, printable.
fn short(id: &str) -> String {
    printable(&id.chars().take(SHORT_ID).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `op`'s text for `result`, a result object as 0.11.0 printed it.
    fn shaped(op: &str, result: &str) -> Option<String> {
        let result: Value = serde_json::from_str(result).expect("a test result is JSON");
        text(op, &result)
    }

    /// A workspace two changesets behind `main`, in a rebase, with one change of each kind.
    const STATUS: &str = r#"{"name":"w",
        "base":"8fea89701cbc90c8b4713d8c83c47c370c290ba9c0d6b46640278a77135a6746",
        "head":"8fea89701cbc90c8b4713d8c83c47c370c290ba9c0d6b46640278a77135a6746",
        "main":"325a37e6f8f1da4761dba516623953f45b3b235ef349854ed3b4f47cb2c46083",
        "behind_by":2,"state":"rebasing",
        "changes":[{"path":"a.txt","kind":"added","size":4},
            {"path":"base.txt","kind":"modified","size":19},
            {"path":"gone.txt","kind":"deleted"},
            {"path":"run.sh","kind":"mode_changed","size":9},
            {"path":"link","kind":"type_changed","size":3}],
        "rebasing":{"onto":"325a37e6f8f1da4761dba516623953f45b3b235ef349854ed3b4f47cb2c46083",
            "conflicts":[]},
        "truncated":true}"#;

    /// A clean workspace at `main`'s head.
    const CLEAN: &str = r#"{"name":"w",
        "base":"8fea89701cbc90c8b4713d8c83c47c370c290ba9c0d6b46640278a77135a6746",
        "head":"8fea89701cbc90c8b4713d8c83c47c370c290ba9c0d6b46640278a77135a6746",
        "main":"8fea89701cbc90c8b4713d8c83c47c370c290ba9c0d6b46640278a77135a6746",
        "behind_by":0,"state":"clean","changes":[],"truncated":false}"#;

    /// Two changesets, newest first, with more to page.
    const LOG: &str = r#"{"entries":[
        {"id":"8fea89701cbc90c8b4713d8c83c47c370c290ba9c0d6b46640278a77135a6746",
            "parents":["325a37e6f8f1da4761dba516623953f45b3b235ef349854ed3b4f47cb2c46083"],
            "author_agent":"t/a","created_at":1794445505000,"title":"add base"},
        {"id":"325a37e6f8f1da4761dba516623953f45b3b235ef349854ed3b4f47cb2c46083",
            "parents":[],"author_agent":"t/a","created_at":1794445500000,"title":"init"}],
        "truncated":true}"#;

    #[test]
    fn status_is_a_header_and_one_line_a_change() {
        assert_eq!(
            shaped("ws_status", STATUS).as_deref(),
            Some(
                "## w 8fea89701cbc behind main by 2 rebasing\n\
                 A  a.txt\n\
                 M  base.txt\n\
                 D  gone.txt\n\
                 M  run.sh\n\
                 T  link\n\
                 (truncated)\n"
            )
        );
    }

    #[test]
    fn a_clean_status_says_so() {
        assert_eq!(
            shaped("ws_status", CLEAN).as_deref(),
            Some("## w 8fea89701cbc\nnothing to commit, workspace clean\n")
        );
    }

    #[test]
    fn an_unknown_change_kind_prints_a_question_mark() {
        let status = CLEAN.replace(
            r#""changes":[]"#,
            r#""changes":[{"path":"x","kind":"renamed"}]"#,
        );
        assert_eq!(
            shaped("ws_status", &status).as_deref(),
            Some("## w 8fea89701cbc\n?  x\n")
        );
    }

    #[test]
    fn log_is_one_line_a_changeset() {
        assert_eq!(
            shaped("log", LOG).as_deref(),
            Some("8fea89701cbc add base\n325a37e6f8f1 init\n(truncated)\n")
        );
    }

    #[test]
    fn another_operation_has_no_shape() {
        assert_eq!(shaped("ws_info", CLEAN), None);
        assert_eq!(shaped("read_file", LOG), None);
    }

    #[test]
    fn a_result_without_a_field_has_no_shape() {
        assert_eq!(
            shaped("ws_status", &CLEAN.replace(r#""head""#, r#""tip""#)),
            None
        );
        assert_eq!(
            shaped("log", &LOG.replace(r#""title""#, r#""subject""#)),
            None
        );
    }

    #[test]
    fn a_control_character_is_escaped() {
        let status = CLEAN.replace(
            r#""changes":[]"#,
            r#""changes":[{"path":"a\nb","kind":"added"}]"#,
        );
        assert_eq!(
            shaped("ws_status", &status).as_deref(),
            Some("## w 8fea89701cbc\nA  a\\nb\n")
        );
        let log = LOG.replace("add base", r"add \u001b[31mbase");
        assert_eq!(
            shaped("log", &log).as_deref(),
            Some("8fea89701cbc add \\u{1b}[31mbase\n325a37e6f8f1 init\n(truncated)\n")
        );
    }
}
