//! The page "Coming from git" against the binary: each row of the committed
//! `docs/reference/coming-from-git.md` is run as `so` in a fixture, so the page cannot say what
//! the binary does not do.
//!
//! ```text
//! coming-from-git.md ─▶ rows ─▶ `so <example>` at the workspace's root
//!                                 ├─ a row with an `op`: that operation ran, no usage error
//!                                 └─ a row without: exit 2, the row's hint first
//! ```
//!
//! The fixture is a store whose `main` holds two changesets, and workspace `w` of it with an
//! edit in `a.txt`. No test opens a connection: `clone` runs with no identity, which is refused
//! first, and the store has no remote.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::support::{self, SO};
use crate::ws::{AGENT, Lab, write};

/// The page, from this crate's directory.
const PAGE: &str = "../../docs/reference/coming-from-git.md";

/// The header of a tier's table on the page.
const HEADER: &str = "| git | Class | What to type | `op` | Example | Hint when refused |";

/// How many cells a row has.
const CELLS: usize = 6;

/// A cell with nothing in it.
const EMPTY: &str = "—";

/// How many git verbs each tier's table holds: 29 everyday ones, 20 of scripts and CI.
const TIER_ROWS: [usize; 2] = [29, 20];

/// The verbs whose example runs with no identity, and is refused for that before it connects.
const NO_IDENTITY: &[&str] = &["clone"];

/// The error a usage refusal carries, and its exit code.
const USAGE_CODE: &str = "E_INVALID_ARG";
const USAGE_EXIT: i32 = 2;

/// The error of a line that needs an identity and has none.
const IDENTITY_CODE: &str = "E_IDENTITY_REQUIRED";

/// One row of the page: what the test runs and what it expects.
#[derive(Clone, Debug)]
struct PageRow {
    /// git's verb.
    git: String,
    /// The envelope's `op` of the example, for a verb that runs.
    op: Option<String>,
    /// The example's words after the program's name.
    example: Vec<String>,
    /// The first line of the refusal's hint.
    hint: Option<String>,
}

/// `cell` without its code span; `None` for an empty cell.
fn span(cell: &str) -> Option<String> {
    if cell == EMPTY {
        return None;
    }
    let inner = cell.strip_prefix('`').and_then(|c| c.strip_suffix('`'));
    Some(
        inner
            .unwrap_or_else(|| panic!("{cell:?} is one code span"))
            .to_owned(),
    )
}

/// One table line as a row.
fn page_row(line: &str) -> PageRow {
    let inner = line.strip_prefix("| ").and_then(|l| l.strip_suffix(" |"));
    let cells: Vec<&str> = inner.unwrap_or(line).split(" | ").collect();
    assert_eq!(cells.len(), CELLS, "{line}");

    // The verb and the example are written with their programs' names.
    let git = span(cells[0]).expect("a git command");
    let example = span(cells[4]).expect("an example");
    PageRow {
        git: git.strip_prefix("git ").expect("`git <verb>`").to_owned(),
        op: span(cells[3]),
        example: (example.split_whitespace().skip(1))
            .map(str::to_owned)
            .collect(),
        hint: span(cells[5]),
    }
}

/// The rows of each table of `page` that starts with [`HEADER`].
fn tables(page: &str) -> Vec<Vec<PageRow>> {
    let mut tables = Vec::new();
    let mut lines = page.lines();
    while let Some(line) = lines.next() {
        if line != HEADER {
            continue;
        }

        // The separator, then one row a line up to the blank line.
        let rows = (lines.by_ref().skip(1))
            .take_while(|line| line.starts_with('|'))
            .map(page_row);
        tables.push(rows.collect());
    }
    tables
}

/// The fixture: the store and its workspace `w`.
struct Fixture {
    _lab: Lab,
    ws: PathBuf,
}

impl Fixture {
    /// A store whose `main` holds two changesets, `a.txt` in the second, and workspace `w`
    /// with `a.txt` edited.
    fn new() -> Fixture {
        let lab = Lab::new();
        lab.publish("a.txt", "a\n");
        let ws = lab.workspace("w");
        write(&ws, "a.txt", "edited\n");
        Fixture { _lab: lab, ws }
    }
}

/// What a run printed and how it ended.
struct Out {
    exit: i32,
    stdout: String,
}

/// Runs `so` at `dir` with `words` and none of this process's `SWARMOPS_*` variables.
fn so_at(dir: &Path, words: &[&str]) -> Out {
    let mut command = Command::new(SO);
    for name in support::swarmops_vars() {
        command.env_remove(name);
    }
    command.args(words).current_dir(dir).stdin(Stdio::null());
    let done = command.output().expect("run so");
    Out {
        exit: done.status.code().expect("so exited"),
        stdout: String::from_utf8_lossy(&done.stdout).into_owned(),
    }
}

/// What of `rows` does not hold in the binary, run in order at the workspace's root of `fx`:
/// one line a row that fails, empty when the page is right.
fn failures(fx: &Fixture, rows: &[PageRow]) -> Vec<String> {
    rows.iter().filter_map(|row| failure(fx, row)).collect()
}

/// Why `row` does not hold, if it does not.
fn failure(fx: &Fixture, row: &PageRow) -> Option<String> {
    // 1. The example, as the agent unless the row runs without an identity.
    let anonymous = row.op.is_none() || NO_IDENTITY.contains(&row.git.as_str());
    let mut words = Vec::new();
    if !anonymous {
        words.extend(["--as", AGENT]);
    }
    words.extend(row.example.iter().map(String::as_str));
    let out = so_at(&fx.ws, &words);
    let said = format!("git {}: so {}", row.git, row.example.join(" "));
    let Ok(envelope) = serde_json::from_str::<Value>(&out.stdout) else {
        return Some(format!("{said}: stdout is no envelope: {}", out.stdout));
    };
    let code = envelope["error"]["code"].as_str().unwrap_or_default();

    // 2. A verb that runs: its operation, and no usage refusal.
    if let Some(op) = &row.op {
        if envelope["op"] != op.as_str() {
            return Some(format!("{said}: op {}, the page says {op}", envelope["op"]));
        }
        if code == USAGE_CODE {
            return Some(format!("{said}: refused as a usage error"));
        }
        let unasked = NO_IDENTITY.contains(&row.git.as_str()) && code != IDENTITY_CODE;
        return unasked.then(|| format!("{said}: {code:?}, not {IDENTITY_CODE}"));
    }

    // 3. A refused verb: a usage refusal with the row's hint first.
    let Some(hint) = &row.hint else {
        return Some(format!("{said}: the row has no op and no hint"));
    };
    if (out.exit, code) != (USAGE_EXIT, USAGE_CODE) {
        return Some(format!("{said}: exit {} {code:?}, not refused", out.exit));
    }
    let first = envelope["error"]["hint"].as_str().unwrap_or_default();
    let first = first.lines().next().unwrap_or_default();
    (first != hint).then(|| format!("{said}: hint {first:?}, the page says {hint:?}"))
}

/// Every row of the committed page holds in the binary: a row with an `op` runs that operation
/// and is no usage error, a row without one is refused, exit 2, with its hint first.
#[test]
fn every_page_row_holds_in_the_binary() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(PAGE);
    let page = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{PAGE}: {e}"));
    let tables = tables(&page);

    // 1. Both tiers, every verb of each.
    let counts: Vec<usize> = tables.iter().map(Vec::len).collect();
    assert_eq!(counts, TIER_ROWS, "the rows of the page's tables");

    // 2. Each row, in the page's order.
    let rows: Vec<PageRow> = tables.into_iter().flatten().collect();
    let failed = failures(&Fixture::new(), &rows);
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

/// The check itself: a row with a wrong `op`, one with a wrong hint and one with neither are
/// each reported, and a right row is not.
#[test]
fn a_planted_row_is_caught() {
    let table = [
        HEADER,
        "| --- | --- | --- | --- | --- | --- |",
        "| `git status` | A | `so status` | `ws_status` | `so status` | — |",
        "| `git status` | A | `so status` | `log` | `so status` | — |",
        "| `git add` | N | — | — | `so add a.txt` | `git add: stage it` |",
        "| `git bisect` | N | — | — | `so bisect start` | — |",
        "",
    ];
    let tables = tables(&table.join("\n"));
    let [rows] = tables.as_slice() else {
        panic!("one table, not {}", tables.len());
    };

    let failed = failures(&Fixture::new(), rows);
    let [op, hint, neither] = failed.as_slice() else {
        panic!("three rows fail, not {failed:?}");
    };
    assert!(op.contains("the page says log"), "{op}");
    assert!(
        hint.contains("the page says \"git add: stage it\""),
        "{hint}"
    );
    assert!(neither.contains("no op and no hint"), "{neither}");
}
