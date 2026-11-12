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
        "diff" | "ws_diff" => diff(result),
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

/// What a diff's files carry, which picks git's shape for them.
#[derive(Clone, Copy)]
enum Carried {
    /// Hunks (`--format unified`): a patch.
    Hunks,
    /// Counts and no hunks (`--format stat`): git's `--numstat`.
    Counts,
    /// Paths and statuses only (`--format names`): git's `--name-status`.
    Names,
}

/// The side of a patch a file is named on, `/dev/null` where it has none.
const NO_SIDE: &str = "/dev/null";

/// The line git writes after a patch line that has no line break.
const NO_NEWLINE: &str = "\\ No newline at end of file\n";

/// `diff` and `ws diff` in git's shape for what their files carry.
fn diff(result: &Value) -> Option<String> {
    let files = result.get("files")?.as_array()?;
    let truncated = result.get("truncated")?.as_bool()?;

    // 1. One shape for the whole diff: a unified diff carries each file's kinds, a stat diff
    //    its counts or binary flag.
    let any = |key: &str| files.iter().any(|file| file.get(key).is_some());
    let carried = if any("hunks") || any("old_kind") || any("new_kind") {
        Carried::Hunks
    } else if any("insertions") || any("binary") {
        Carried::Counts
    } else {
        Carried::Names
    };

    // 2. Each file in that shape.
    let mut out = String::new();
    for file in files {
        let path = printable(string(file, "path")?);
        let status = string(file, "status")?;
        match carried {
            Carried::Hunks => patch(&mut out, file, &path, status)?,
            Carried::Counts => numstat(&mut out, file, &path)?,
            Carried::Names => {
                let _ = writeln!(out, "{}\t{path}", letter(status));
            }
        }
    }

    if truncated {
        out.push_str(TRUNCATED);
    }
    Some(out)
}

/// One file of a unified diff: `--- a/<path>`, `+++ b/<path>` and its hunks, or one line for a
/// binary file. `path` is printable already.
fn patch(out: &mut String, file: &Value, path: &str, status: &str) -> Option<()> {
    let old = match status {
        "added" => NO_SIDE.to_owned(),
        _ => format!("a/{path}"),
    };
    let new = match status {
        "deleted" => NO_SIDE.to_owned(),
        _ => format!("b/{path}"),
    };

    // 1. A binary file is named, its bytes are not printed.
    if file.get("binary").and_then(Value::as_bool) == Some(true) {
        let _ = writeln!(out, "Binary files {old} and {new} differ");
        return Some(());
    }
    let _ = writeln!(out, "--- {old}\n+++ {new}");

    // 2. Each hunk: its header, then a line a side, by its sign.
    let Some(hunks) = file.get("hunks") else {
        return Some(());
    };
    for hunk in hunks.as_array()? {
        let number = |key: &str| hunk.get(key)?.as_u64();
        let _ = writeln!(
            out,
            "@@ -{},{} +{},{} @@",
            number("old_start")?,
            number("old_lines")?,
            number("new_start")?,
            number("new_lines")?
        );
        for line in hunk.get("lines")?.as_array()? {
            let sign = match string(line, "op")? {
                "context" => ' ',
                "add" => '+',
                "remove" => '-',
                _ => return None,
            };
            let text = string(line, "text")?;
            let body = text.strip_suffix('\n');
            let _ = writeln!(out, "{sign}{}", printable(body.unwrap_or(text)));
            if body.is_none() {
                out.push_str(NO_NEWLINE);
            }
        }
    }
    Some(())
}

/// One file of a stat diff as git's `--numstat`: `<insertions>\t<deletions>\t<path>`, or
/// `-\t-\t<path>` for a binary file. `path` is printable already.
fn numstat(out: &mut String, file: &Value, path: &str) -> Option<()> {
    if file.get("binary").and_then(Value::as_bool) == Some(true) {
        let _ = writeln!(out, "-\t-\t{path}");
        return Some(());
    }
    let insertions = file.get("insertions")?.as_u64()?;
    let deletions = file.get("deletions")?.as_u64()?;
    let _ = writeln!(out, "{insertions}\t{deletions}\t{path}");
    Some(())
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

    /// `ws diff --format unified`: an added, a modified, a binary and a deleted file, and one
    /// whose last line has no line break.
    const UNIFIED: &str = r#"{"from":"8fea89701cbc90c8b4713d8c83c47c370c290ba9c0d6b46640278a77135a6746",
        "to":"ws:w","files":[
        {"path":"a.txt","status":"added","new_kind":"file",
            "new_blob":"48827a08c982ae504244a19676578414067f55fc82a3f51f9a21c7b393c2ce9e",
            "binary":false,"insertions":2,"deletions":0,
            "hunks":[{"old_start":0,"old_lines":0,"new_start":1,"new_lines":2,
                "lines":[{"op":"add","text":"a\n"},{"op":"add","text":"b\n"}]}]},
        {"path":"base.txt","status":"modified","old_kind":"file","new_kind":"file",
            "old_blob":"0e1243b20b2778397ceb2e51b2b8b777123ad5775129cffce5e8a7054f0bd3de",
            "new_blob":"a4fb70ffb85736c1749c46f97382787fbb879e9144b52d2b3ceb5c7bd921314c",
            "binary":false,"insertions":1,"deletions":1,
            "hunks":[{"old_start":1,"old_lines":2,"new_start":1,"new_lines":2,
                "lines":[{"op":"context","text":"base\n"},{"op":"remove","text":"line2\n"},
                    {"op":"add","text":"line2 changed\n"}]}]},
        {"path":"bin.dat","status":"added","new_kind":"file",
            "new_blob":"0b99e091eb92d153a8ecbd4ce658a4ad6458c7a876a5977078166ac191b19b13",
            "binary":true},
        {"path":"gone.txt","status":"deleted","old_kind":"file",
            "old_blob":"ad3072fbe455796b1d201bf3e9212f6fe7996e8208b38e3c58114614310ad53d",
            "binary":false,"insertions":0,"deletions":1,
            "hunks":[{"old_start":1,"old_lines":1,"new_start":0,"new_lines":0,
                "lines":[{"op":"remove","text":"bye\n"}]}]},
        {"path":"tail.txt","status":"modified","old_kind":"file","new_kind":"file",
            "old_blob":"e9e549084200febf5dec6fcf4be0aca8a29468a38b3979b590aeaacd98b22e37",
            "new_blob":"78e9243b15e05d5c93202428d1ede8efb22b63f0204f43133e27a88a5a83d2d4",
            "binary":false,"insertions":1,"deletions":1,
            "hunks":[{"old_start":1,"old_lines":1,"new_start":1,"new_lines":1,
                "lines":[{"op":"remove","text":"old\n"},{"op":"add","text":"new"}]}]}],
        "truncated":false}"#;

    /// `ws diff --format stat`: the same files, counted.
    const STAT: &str = r#"{"from":"8fea89701cbc90c8b4713d8c83c47c370c290ba9c0d6b46640278a77135a6746",
        "to":"ws:w","files":[
        {"path":"a.txt","status":"added","binary":false,"insertions":2,"deletions":0},
        {"path":"base.txt","status":"modified","binary":false,"insertions":1,"deletions":1},
        {"path":"bin.dat","status":"added","binary":true},
        {"path":"gone.txt","status":"deleted","binary":false,"insertions":0,"deletions":1}],
        "truncated":false}"#;

    /// `ws diff`: the same files, named; one a kind this build does not know, and more to page.
    const NAMES: &str = r#"{"from":"8fea89701cbc90c8b4713d8c83c47c370c290ba9c0d6b46640278a77135a6746",
        "to":"ws:w","files":[
        {"path":"a.txt","status":"added"},{"path":"base.txt","status":"modified"},
        {"path":"gone.txt","status":"deleted"},{"path":"link","status":"type_changed"},
        {"path":"x","status":"renamed"}],
        "truncated":true}"#;

    #[test]
    fn a_unified_diff_is_a_patch() {
        assert_eq!(
            shaped("ws_diff", UNIFIED).as_deref(),
            Some(
                "--- /dev/null\n\
                 +++ b/a.txt\n\
                 @@ -0,0 +1,2 @@\n\
                 +a\n\
                 +b\n\
                 --- a/base.txt\n\
                 +++ b/base.txt\n\
                 @@ -1,2 +1,2 @@\n \
                 base\n\
                 -line2\n\
                 +line2 changed\n\
                 Binary files /dev/null and b/bin.dat differ\n\
                 --- a/gone.txt\n\
                 +++ /dev/null\n\
                 @@ -1,1 +0,0 @@\n\
                 -bye\n\
                 --- a/tail.txt\n\
                 +++ b/tail.txt\n\
                 @@ -1,1 +1,1 @@\n\
                 -old\n\
                 +new\n\
                 \\ No newline at end of file\n"
            )
        );
    }

    #[test]
    fn a_stat_diff_is_numstat() {
        assert_eq!(
            shaped("diff", STAT).as_deref(),
            Some("2\t0\ta.txt\n1\t1\tbase.txt\n-\t-\tbin.dat\n0\t1\tgone.txt\n")
        );
    }

    #[test]
    fn a_names_diff_is_name_status() {
        assert_eq!(
            shaped("ws_diff", NAMES).as_deref(),
            Some("A\ta.txt\nM\tbase.txt\nD\tgone.txt\nT\tlink\n?\tx\n(truncated)\n")
        );
    }

    #[test]
    fn a_binary_file_is_named_not_printed() {
        let deleted = r#"{"from":"a","to":"b","files":[{"path":"bin.dat","status":"deleted",
            "old_kind":"file","old_blob":"0b99e091eb92","binary":true}],"truncated":false}"#;
        assert_eq!(
            shaped("diff", deleted).as_deref(),
            Some("Binary files a/bin.dat and /dev/null differ\n")
        );
        let modified = deleted.replace("deleted", "modified");
        assert_eq!(
            shaped("diff", &modified).as_deref(),
            Some("Binary files a/bin.dat and b/bin.dat differ\n")
        );
    }

    #[test]
    fn a_diff_without_a_field_has_no_shape() {
        assert_eq!(
            shaped("diff", &STAT.replace(r#""insertions":2,"#, "")),
            None
        );
        assert_eq!(
            shaped("ws_diff", &UNIFIED.replace(r#""old_start""#, r#""start""#)),
            None
        );
        assert_eq!(
            shaped(
                "ws_diff",
                &UNIFIED.replace(r#""op":"add""#, r#""op":"swap""#)
            ),
            None
        );
    }

    #[test]
    fn a_control_character_in_a_diff_is_escaped() {
        let patch = UNIFIED.replace(r"line2 changed\n", r"line2\r\n");
        let text = shaped("ws_diff", &patch).expect("a patch");
        assert!(text.contains("\n+line2\\r\n"), "{text}");
        let names = NAMES.replace(r#""path":"x""#, r#""path":"x\ty""#);
        let text = shaped("ws_diff", &names).expect("a listing");
        assert!(text.contains("?\tx\\ty\n"), "{text}");
    }
}
