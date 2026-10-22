//! `docs/reference/limits.md`: every limit of the contract, with the constant that sets it, its
//! value and unit, the commands that enforce it and the error a request past it gets; the bounds
//! a response keeps without an error; the paging defaults; the bounds the JSON Schema states; and,
//! apart, internal values that are not part of the contract.
//!
//! ```text
//! CONTRACT, BOUNDS (checked against the code) ─┬─▶ limits.md
//! DEFAULTS (checked against clap and schema) ──┤
//! schema_for_all ──────────────────────────────┤   Bounds in the JSON Schema
//! INTERNAL (checked against the code) ─────────┤   Internal values
//! notes/limits.md ─────────────────────────────┘   Details
//! ```
//!
//! A value is the constant's own where the constant is public, and otherwise the value its
//! declaration in the source gives: `values_match_the_code` reads each declaration and fails when
//! the value here is not its value.

use std::fmt::Write as _;
use std::fs;

use serde_json::Value;
use swarmops_api::{ErrorCode, schema_for_all};
use swarmops_core::bytes::to_u64;

use super::command_page::{self, file_name};
use super::{GENERATED, Page, all_leaves, cell, paragraphs, reference_dir, slug, trim_end};

/// The page's name in `docs/reference/`.
pub(super) const FILE: &str = "limits.md";

/// The hand-written note spliced under `## Details`, in `docs/reference/`.
const NOTE: &str = "notes/limits.md";

/// The schema-types page, which the schema bounds link to.
const SCHEMA_TYPES: &str = "schema-types.md";

/// One binary unit of bytes: a kibibyte.
const KIB: u64 = 1024;

/// What a value counts.
#[derive(Clone, Copy)]
enum Unit {
    /// Bytes, shown also in KiB, MiB or GiB when a whole number of them.
    Bytes,
    /// Seconds.
    Seconds,
    /// Milliseconds.
    Millis,
    /// Things of one kind, by their plural: `levels`, `attempts`.
    Count(&'static str),
}

/// Where a row's value comes from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// The public constant itself, read by this code.
    Code,
    /// The declaration in the source, which the test evaluates: the constant is private.
    Declared,
}

/// A constant: its path, its source file (from the repository root), its declaration as the
/// source writes it, and its value.
struct Constant {
    path: &'static str,
    file: &'static str,
    decl: &'static str,
    value: fn() -> u64,
    origin: Origin,
}

impl Constant {
    /// Its value.
    fn value(&self) -> u64 {
        (self.value)()
    }
}

/// A limit: its name, its constant and unit, the commands that enforce it, the errors a request
/// past it gets (none for a bound that cuts a response short instead), and what it bounds.
struct Limit {
    name: &'static str,
    constant: Constant,
    unit: Unit,
    commands: &'static [&'static str],
    errors: &'static [ErrorCode],
    what: &'static str,
}

/// A paging default: the request type and field it fills, the constant, the command and the
/// flag that sets it on the command line (none when the command has no flag).
struct DefaultRow {
    request: &'static str,
    field: &'static str,
    constant: Constant,
    unit: Unit,
    command: &'static str,
    flag: Option<&'static str>,
}

/// A constant whose value this code reads.
const fn code(
    path: &'static str,
    file: &'static str,
    decl: &'static str,
    value: fn() -> u64,
) -> Constant {
    Constant {
        path,
        file,
        decl,
        value,
        origin: Origin::Code,
    }
}

/// A private constant, whose value its declaration gives.
const fn declared(
    path: &'static str,
    file: &'static str,
    decl: &'static str,
    value: fn() -> u64,
) -> Constant {
    Constant {
        path,
        file,
        decl,
        value,
        origin: Origin::Declared,
    }
}

/// The file that declares the paging defaults.
const API: &str = "crates/swarmops-api/src/ops.rs";

/// A paging default of `command`'s `flag`, filling `request`'s `field`.
const fn paged(
    request: &'static str,
    field: &'static str,
    constant: Constant,
    unit: Unit,
    command: &'static str,
    flag: Option<&'static str>,
) -> DefaultRow {
    DefaultRow {
        request,
        field,
        constant,
        unit,
        command,
        flag,
    }
}

/// An internal value: no command enforces it, and no request gets an error of it.
const fn internal_value(
    name: &'static str,
    constant: Constant,
    unit: Unit,
    what: &'static str,
) -> Limit {
    Limit {
        name,
        constant,
        unit,
        commands: &[],
        errors: &[],
        what,
    }
}

// ---- The tables ----

/// The limits a request is refused past, with the error it gets.
static CONTRACT: [Limit; 10] = [
    Limit {
        name: "Request text",
        constant: code(
            "swarmops_ops::content::MAX_REQUEST_BYTES",
            "crates/swarmops-ops/src/content.rs",
            "pub const MAX_REQUEST_BYTES: u64 = 64 << 20;",
            || swarmops_ops::content::MAX_REQUEST_BYTES,
        ),
        unit: Unit::Bytes,
        commands: &["commit", "land", "mcp"],
        errors: &[ErrorCode::TooLarge],
        what: "The text of a request that a client reads: `commit`'s `--changes` file or \
                   stdin, `land`'s `--resolve` file, and each message line `mcp` reads. The \
                   command line refuses it before parsing it, with `details.scope` `request`; \
                   `mcp` answers a longer line with a JSON-RPC Invalid Request without an `id`, \
                   and reads on at the next line.",
    },
    Limit {
        name: "One content",
        constant: code(
            "swarmops_ops::content::MAX_CONTENT_BYTES",
            "crates/swarmops-ops/src/content.rs",
            "pub const MAX_CONTENT_BYTES: u64 = swarmops_store::MAX_OBJECT_LEN;",
            || swarmops_ops::content::MAX_CONTENT_BYTES,
        ),
        unit: Unit::Bytes,
        commands: &["commit", "land", "blob put"],
        errors: &[ErrorCode::TooLarge],
        what: "One put's content, inline, base64 (by the bytes it decodes to) or a file, in \
                   a commit's change list, a land's resolutions or a `blob put`: the most an \
                   object of the store holds. It is measured before it is converted, with \
                   `details.scope` `put`; a blob named by id counts nothing.",
    },
    Limit {
        name: "A commit's contents",
        constant: code(
            "swarmops_ops::content::MAX_COMMIT_CONTENT_BYTES",
            "crates/swarmops-ops/src/content.rs",
            "pub const MAX_COMMIT_CONTENT_BYTES: u64 = 1 << 30;",
            || swarmops_ops::content::MAX_COMMIT_CONTENT_BYTES,
        ),
        unit: Unit::Bytes,
        commands: &["commit", "land"],
        errors: &[ErrorCode::TooLarge],
        what: "The contents of a commit's change list together, or of a land's \
                   resolutions, with `details.scope` `commit`. The sizes the contents declare \
                   are checked before any file is read; a blob named by id counts nothing.",
    },
    Limit {
        name: "An object",
        constant: code(
            "swarmops_store::MAX_OBJECT_LEN",
            "crates/swarmops-store/src/fs_objects.rs",
            "pub const MAX_OBJECT_LEN: u64 = 512 << 20;",
            || swarmops_store::MAX_OBJECT_LEN,
        ),
        unit: Unit::Bytes,
        commands: &["commit"],
        errors: &[ErrorCode::TooLarge],
        what: "An object's payload in the store, with `details.store_limit` \
                   `object_bytes`: a commit's tree or changeset object past it, since a put's \
                   content is refused at the same size first.",
    },
    Limit {
        name: "Tree depth",
        constant: code(
            "swarmops_api::MAX_TREE_DEPTH",
            "crates/swarmops-api/src/ops.rs",
            "pub const MAX_TREE_DEPTH: u32 = 64;",
            || u64::from(swarmops_api::MAX_TREE_DEPTH),
        ),
        unit: Unit::Count("levels"),
        commands: &["ls"],
        errors: &[ErrorCode::InvalidArg],
        what: "How many levels `ls --depth` (`read_tree`'s `depth`) lists below a \
                   directory: 1 to this many, with `details.depth`.",
    },
    Limit {
        name: "Land attempts",
        constant: code(
            "swarmops_api::MAX_LAND_ATTEMPTS",
            "crates/swarmops-api/src/ops.rs",
            "pub const MAX_LAND_ATTEMPTS: u32 = 32;",
            || u64::from(swarmops_api::MAX_LAND_ATTEMPTS),
        ),
        unit: Unit::Count("attempts"),
        commands: &["land"],
        errors: &[ErrorCode::InvalidArg],
        what: "`land --max-attempts` (`max_attempts`): 1 to this many, checked before \
                   anything is resolved or read, with `details.max_attempts`.",
    },
    Limit {
        name: "Land key",
        constant: declared(
            "swarmops_ops::local::MAX_LAND_KEY",
            "crates/swarmops-ops/src/local.rs",
            "const MAX_LAND_KEY: usize = 64;",
            || 64,
        ),
        unit: Unit::Count("characters"),
        commands: &["land"],
        errors: &[ErrorCode::InvalidArg],
        what: "`land --key` (`idempotency_key`): 1 to this many ASCII letters, digits and \
                   `-`, a uuid v7 or a changeset id's 64 hex digits, with \
                   `details.idempotency_key`.",
    },
    Limit {
        name: "Agent name",
        constant: declared(
            "swarmops_api::identity::MAX_AGENT_LEN",
            "crates/swarmops-api/src/identity.rs",
            "const MAX_AGENT_LEN: usize = 255;",
            || 255,
        ),
        unit: Unit::Bytes,
        commands: &[
            "whoami",
            "commit",
            "land",
            "blob put",
            "ref set",
            "ref delete",
            "op undo",
            "git import",
            "mcp",
        ],
        errors: &[ErrorCode::InvalidArg],
        what: "The agent name of `--as` or `SWARMOPS_AGENT`, `<family>/<instance>`, which a \
                   ref move records with a one-byte length. `details.source` names where the \
                   name came from.",
    },
    Limit {
        name: "Ref name",
        constant: declared(
            "swarmops_store::meta::refs::MAX_NAME_LEN",
            "crates/swarmops-store/src/meta/refs.rs",
            "const MAX_NAME_LEN: usize = 255;",
            || 255,
        ),
        unit: Unit::Bytes,
        commands: &[
            "commit",
            "land",
            "ref set",
            "ref delete",
            "git import",
            "git export",
        ],
        errors: &[ErrorCode::RefNameInvalid],
        what: "A ref's name, `main` or `release/<x>`, which a ref move records with a \
                   one-byte length: `commit --advance`, `land --target`, `ref set`, `ref \
                   delete`, `git import --into` and `git export --ref`.",
    },
    Limit {
        name: "Repository name",
        constant: declared(
            "swarmops_repo::repo::MAX_NAME_LEN",
            "crates/swarmops-repo/src/repo.rs",
            "const MAX_NAME_LEN: usize = 255;",
            || 255,
        ),
        unit: Unit::Bytes,
        commands: &["init"],
        errors: &[ErrorCode::InvalidArg],
        what: "`init --name`, refused before anything is created, with \
                   `details.store_limit` `repo_name_bytes`.",
    },
];

/// The bounds a response keeps without an error: what it leaves out, and how it says so.
static BOUNDS: [Limit; 5] = [
    Limit {
        name: "Lossy commits listed",
        constant: code(
            "swarmops_git::import::MAX_LOSSY_LISTED",
            "crates/swarmops-git/src/import.rs",
            "pub const MAX_LOSSY_LISTED: usize = 100;",
            || to_u64(swarmops_git::import::MAX_LOSSY_LISTED),
        ),
        unit: Unit::Count("commits"),
        commands: &["git import"],
        errors: &[],
        what: "`git import`'s `lossy` lists the first this many commits it took lossily, \
                   in the order imported; `lossy_total` counts them all.",
    },
    Limit {
        name: "Line diff lines",
        constant: code(
            "swarmops_merge::line_diff::MAX_DIFF_LINES",
            "crates/swarmops-merge/src/line_diff.rs",
            "pub const MAX_DIFF_LINES: usize = 48 << 10;",
            || to_u64(swarmops_repo::merge::line_diff::MAX_DIFF_LINES),
        ),
        unit: Unit::Count("lines"),
        commands: &["diff", "land"],
        errors: &[],
        what: "The most lines a side of a file may have to be diffed by lines. Past it, \
                   `diff` gives the file no line counts and no hunks, with `hunks_truncated`, \
                   and a land that must merge the file by lines reports a `binary` conflict.",
    },
    Limit {
        name: "Line diff bytes",
        constant: code(
            "swarmops_merge::line_diff::MAX_DIFF_BYTES",
            "crates/swarmops-merge/src/line_diff.rs",
            "pub const MAX_DIFF_BYTES: usize = 8 << 20;",
            || to_u64(swarmops_repo::merge::line_diff::MAX_DIFF_BYTES),
        ),
        unit: Unit::Bytes,
        commands: &["diff", "land"],
        errors: &[],
        what: "The most bytes a side of a file may have to be diffed by lines, as for the \
                   lines above.",
    },
    Limit {
        name: "Text probe",
        constant: declared(
            "swarmops_merge::line_diff::TEXT_PROBE",
            "crates/swarmops-merge/src/line_diff.rs",
            "const TEXT_PROBE: usize = 8_000;",
            || 8_000,
        ),
        unit: Unit::Bytes,
        commands: &["diff", "land"],
        errors: &[],
        what: "How many leading bytes are searched for a NUL, as git does: a side that is \
                   not UTF-8, or holds a NUL there, is binary, which gets no line counts and no \
                   hunks.",
    },
    Limit {
        name: "Conflict report entries",
        constant: declared(
            "swarmops_ops::conflict::MAX_ENTRY_BYTES",
            "crates/swarmops-ops/src/conflict.rs",
            "const MAX_ENTRY_BYTES: u64 = 24 * 1024;",
            || 24 * KIB,
        ),
        unit: Unit::Bytes,
        commands: &["land"],
        errors: &[],
        what: "The most bytes an `E_CONFLICT` report's files and hunks take as JSON, their \
                   texts aside (`--max-conflict-bytes` bounds those). The first file always \
                   goes in; from the first file left out none follows, and `truncated` says \
                   so.",
    },
];

/// The paging defaults a request takes when it does not say.
static DEFAULTS: [DefaultRow; 13] = [
    paged(
        "ReadFileReq",
        "max_bytes",
        code(
            "swarmops_api::DEFAULT_MAX_BYTES",
            API,
            "pub const DEFAULT_MAX_BYTES: u64 = 1024 * 1024;",
            || swarmops_api::DEFAULT_MAX_BYTES,
        ),
        Unit::Bytes,
        "cat",
        Some("max-bytes"),
    ),
    paged(
        "ReadTreeReq",
        "limit",
        code(
            "swarmops_api::DEFAULT_READ_TREE_LIMIT",
            API,
            "pub const DEFAULT_READ_TREE_LIMIT: usize = 250;",
            || to_u64(swarmops_api::DEFAULT_READ_TREE_LIMIT),
        ),
        Unit::Count("entries"),
        "ls",
        Some("limit"),
    ),
    paged(
        "ShowReq",
        "max_changes",
        code(
            "swarmops_api::DEFAULT_MAX_CHANGES",
            API,
            "pub const DEFAULT_MAX_CHANGES: usize = 400;",
            || to_u64(swarmops_api::DEFAULT_MAX_CHANGES),
        ),
        Unit::Count("changes"),
        "show",
        Some("max-changes"),
    ),
    paged(
        "LogReq",
        "limit",
        code(
            "swarmops_api::DEFAULT_LOG_LIMIT",
            API,
            "pub const DEFAULT_LOG_LIMIT: usize = 50;",
            || to_u64(swarmops_api::DEFAULT_LOG_LIMIT),
        ),
        Unit::Count("changesets"),
        "log",
        Some("limit"),
    ),
    paged(
        "PathHistoryReq",
        "limit",
        code(
            "swarmops_api::DEFAULT_PATH_HISTORY_LIMIT",
            API,
            "pub const DEFAULT_PATH_HISTORY_LIMIT: usize = 50;",
            || to_u64(swarmops_api::DEFAULT_PATH_HISTORY_LIMIT),
        ),
        Unit::Count("changesets"),
        "history",
        Some("limit"),
    ),
    paged(
        "DiffReq",
        "limit",
        code(
            "swarmops_api::DEFAULT_DIFF_LIMIT",
            API,
            "pub const DEFAULT_DIFF_LIMIT: usize = 500;",
            || to_u64(swarmops_api::DEFAULT_DIFF_LIMIT),
        ),
        Unit::Count("files"),
        "diff",
        Some("limit"),
    ),
    paged(
        "DiffReq",
        "max_bytes",
        code(
            "swarmops_api::DEFAULT_DIFF_MAX_BYTES",
            API,
            "pub const DEFAULT_DIFF_MAX_BYTES: u64 = 32_768;",
            || swarmops_api::DEFAULT_DIFF_MAX_BYTES,
        ),
        Unit::Bytes,
        "diff",
        Some("max-bytes"),
    ),
    paged(
        "DiffReq",
        "context",
        code(
            "swarmops_api::DEFAULT_DIFF_CONTEXT",
            API,
            "pub const DEFAULT_DIFF_CONTEXT: u32 = 3;",
            || u64::from(swarmops_api::DEFAULT_DIFF_CONTEXT),
        ),
        Unit::Count("lines"),
        "diff",
        Some("context"),
    ),
    paged(
        "LandReq",
        "max_attempts",
        code(
            "swarmops_api::DEFAULT_MAX_ATTEMPTS",
            API,
            "pub const DEFAULT_MAX_ATTEMPTS: u32 = 8;",
            || u64::from(swarmops_api::DEFAULT_MAX_ATTEMPTS),
        ),
        Unit::Count("attempts"),
        "land",
        Some("max-attempts"),
    ),
    paged(
        "LandReq",
        "max_conflict_bytes",
        code(
            "swarmops_api::DEFAULT_MAX_CONFLICT_BYTES",
            API,
            "pub const DEFAULT_MAX_CONFLICT_BYTES: u64 = 32 * 1024;",
            || swarmops_api::DEFAULT_MAX_CONFLICT_BYTES,
        ),
        Unit::Bytes,
        "land",
        Some("max-conflict-bytes"),
    ),
    paged(
        "OpLogReq",
        "limit",
        code(
            "swarmops_api::DEFAULT_OP_LOG_LIMIT",
            API,
            "pub const DEFAULT_OP_LOG_LIMIT: usize = 50;",
            || to_u64(swarmops_api::DEFAULT_OP_LOG_LIMIT),
        ),
        Unit::Count("operations"),
        "op log",
        Some("limit"),
    ),
    paged(
        "OpLogReq",
        "max_changesets",
        code(
            "swarmops_api::DEFAULT_MAX_CHANGESETS",
            API,
            "pub const DEFAULT_MAX_CHANGESETS: usize = 20;",
            || to_u64(swarmops_api::DEFAULT_MAX_CHANGESETS),
        ),
        Unit::Count("changesets"),
        "op log",
        Some("max-changesets"),
    ),
    paged(
        "DeriveReq",
        "limit",
        code(
            "swarmops_api::DEFAULT_DERIVE_LIMIT",
            API,
            "pub const DEFAULT_DERIVE_LIMIT: usize = 1000;",
            || to_u64(swarmops_api::DEFAULT_DERIVE_LIMIT),
        ),
        Unit::Count("frames"),
        "derive",
        None,
    ),
];

/// Values that tune the implementation, or bound the store's layout where no request reaches:
/// no option sets them, and they are not part of the contract.
static INTERNAL: [Limit; 14] = [
    internal_value(
        "Writer lock wait",
        declared(
            "swarmops_store::meta::store::DEFAULT_LOCK_TIMEOUT",
            "crates/swarmops-store/src/meta/store.rs",
            "const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(10);",
            || 10,
        ),
        Unit::Seconds,
        "How long a write waits for the store's lock; a write that waits it out fails \
             with `E_BUSY`.",
    ),
    internal_value(
        "Index sync threads",
        declared(
            "swarmops_store::meta::store::DEFAULT_PARALLELISM",
            "crates/swarmops-store/src/meta/store.rs",
            "const DEFAULT_PARALLELISM: usize = 5;",
            || 5,
        ),
        Unit::Count("threads"),
        "How many threads a write transaction syncs the five index files from.",
    ),
    internal_value(
        "Import frame",
        code(
            "swarmops_git::import::DEFAULT_FRAME_CHANGESETS",
            "crates/swarmops-git/src/import.rs",
            "pub const DEFAULT_FRAME_CHANGESETS: usize = 8_192;",
            || to_u64(swarmops_git::import::DEFAULT_FRAME_CHANGESETS),
        ),
        Unit::Count("changesets"),
        "How many changesets a journal frame of `git import` holds.",
    ),
    internal_value(
        "Import object batch",
        code(
            "swarmops_git::import::DEFAULT_OBJECT_BATCH_BYTES",
            "crates/swarmops-git/src/import.rs",
            "pub const DEFAULT_OBJECT_BATCH_BYTES: usize = 64 << 20;",
            || to_u64(swarmops_git::import::DEFAULT_OBJECT_BATCH_BYTES),
        ),
        Unit::Bytes,
        "How many bytes of new trees and blobs `git import` gathers before it stores \
             them.",
    ),
    internal_value(
        "Import busy retries",
        declared(
            "swarmops_git::import::BUSY_RETRIES",
            "crates/swarmops-git/src/import.rs",
            "const BUSY_RETRIES: usize = 3;",
            || 3,
        ),
        Unit::Count("retries"),
        "How many times `git import` makes a store call again that found the writer lock \
             held for its whole wait.",
    ),
    internal_value(
        "Raw batch",
        code(
            "swarmops_repo::MAX_RAW_BATCH",
            "crates/swarmops-repo/src/raw.rs",
            "pub const MAX_RAW_BATCH: usize = 65_535;",
            || to_u64(swarmops_repo::MAX_RAW_BATCH),
        ),
        Unit::Count("changesets"),
        "The most changesets one journal frame of `git import` holds, whatever its frame \
             size asks.",
    ),
    internal_value(
        "Export branch walk",
        declared(
            "swarmops_git::export::BRANCH_WALK",
            "crates/swarmops-git/src/export.rs",
            "const BRANCH_WALK: usize = 1_000;",
            || 1_000,
        ),
        Unit::Count("commits"),
        "How many first parents of the branch's commit `git export` reads, looking for \
             the export of the head or of an ancestor of it, before it says the branch moved.",
    ),
    internal_value(
        "Export branch lock",
        declared(
            "swarmops_git::gix_sink::LOCK_TIMEOUT",
            "crates/swarmops-git/src/gix_sink.rs",
            "const LOCK_TIMEOUT: Duration = Duration::from_millis(100);",
            || 100,
        ),
        Unit::Millis,
        "How long `git export` tries a branch's lock, git's `core.filesRefLockTimeout` \
             default, so that two exports racing to one branch take turns.",
    ),
    internal_value(
        "Git file read whole",
        declared(
            "swarmops_git::gix_source::MAX_WHOLE_LEN",
            "crates/swarmops-git/src/gix_source.rs",
            "const MAX_WHOLE_LEN: u64 = 16 << 20;",
            || 16 * KIB * KIB,
        ),
        Unit::Bytes,
        "The longest file of the git directory (`HEAD` and other ref files, `config`, \
             `objects/info/alternates`) that `git import` reads into memory whole; a longer \
             one is refused unread.",
    ),
    internal_value(
        "Held conflict hunks",
        declared(
            "swarmops_repo::merge::MAX_HELD_HUNKS",
            "crates/swarmops-repo/src/merge.rs",
            "const MAX_HELD_HUNKS: usize = 128;",
            || 128,
        ),
        Unit::Count("hunks"),
        "The most conflict hunks a merge holds over all its files; past them, each \
             file's hunks are held as one that spans them.",
    ),
    internal_value(
        "MCP line buffer",
        declared(
            "swarmops_cli::mcp::LINE_KEEP",
            "crates/swarmops-cli/src/mcp.rs",
            "const LINE_KEEP: usize = 64 * 1024;",
            || 64 * KIB,
        ),
        Unit::Bytes,
        "The most bytes `mcp`'s line buffer keeps between messages.",
    ),
    internal_value(
        "Parents",
        declared(
            "swarmops_store::meta::parents::MAX_PARENTS",
            "crates/swarmops-store/src/meta/parents.rs",
            "pub(crate) const MAX_PARENTS: usize = 255;",
            || 255,
        ),
        Unit::Count("parents"),
        "The most parents a changeset's record holds (`details.store_limit` `parents`): \
             a commit has one parent.",
    ),
    internal_value(
        "Actor",
        declared(
            "swarmops_store::meta::store::MAX_ACTOR_LEN",
            "crates/swarmops-store/src/meta/store.rs",
            "const MAX_ACTOR_LEN: usize = 255;",
            || 255,
        ),
        Unit::Bytes,
        "The most bytes a ref move's actor takes (`details.store_limit` `actor_bytes`): \
             the agent name's limit keeps a request within it.",
    ),
    internal_value(
        "Records",
        declared(
            "swarmops_store::meta::frame::MAX_RECORDS",
            "crates/swarmops-store/src/meta/frame.rs",
            "pub(crate) const MAX_RECORDS: usize = 65_535;",
            || 65_535,
        ),
        Unit::Count("records"),
        "The most records one journal frame holds (`details.store_limit` `records`).",
    ),
];

// ---- The page ----

/// Renders `limits.md`.
pub(super) fn page() -> Page {
    Page {
        name: FILE.to_owned(),
        text: render(),
    }
}

/// The page's text.
fn render() -> String {
    let mut out = String::new();

    // 1. Title and how to read the page.
    let _ = writeln!(out, "{GENERATED}\n");
    let _ = writeln!(out, "# Limits\n");
    let _ = writeln!(
        out,
        "The limits of the SwarmOps contract: what a request may hold, with the error a request \
         past a limit gets, and how far a response goes before it leaves the rest out. Each \
         limit names the constant that sets it, so a value here is the code's. Then the \
         defaults a paged request takes, the bounds the JSON Schema states, and, apart, \
         internal values that are not part of the contract.\n"
    );

    // 2. The limits a request is refused past.
    let _ = writeln!(out, "## Contract limits\n");
    limits_table(&mut out, &CONTRACT, Column::Error);
    for limit in &CONTRACT {
        limit_section(&mut out, limit);
    }

    // 3. The bounds a response keeps without an error.
    let _ = writeln!(out, "## Bounds without an error\n");
    let _ = writeln!(
        out,
        "A response past these leaves the rest out, and says so in its fields; nothing \
         fails.\n"
    );
    limits_table(&mut out, &BOUNDS, Column::Commands);
    for limit in &BOUNDS {
        limit_section(&mut out, limit);
    }

    // 4. The defaults, the schema's bounds, the internal values, the note.
    defaults_section(&mut out);
    schema_bounds(&mut out);
    internal_section(&mut out);
    details(&mut out);
    trim_end(out)
}

/// The last column of a limits table: the error, or the commands for a bound without one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Column {
    Error,
    Commands,
}

/// The table of `limits`, each linked to its section.
fn limits_table(out: &mut String, limits: &[Limit], last: Column) {
    let header = match last {
        Column::Error => "Error",
        Column::Commands => "Commands",
    };
    let _ = writeln!(out, "| Limit | Value | Constant | {header} |");
    let _ = writeln!(out, "| --- | --- | --- | --- |");
    for limit in limits {
        let tail = match last {
            Column::Error => error_links(limit.errors),
            Column::Commands => command_links(limit.commands),
        };
        let _ = writeln!(
            out,
            "| [{0}](#{1}) | {2} | `{3}` | {tail} |",
            limit.name,
            slug(limit.name),
            amount(limit.constant.value(), limit.unit),
            limit.constant.path
        );
    }
    out.push('\n');
}

/// One limit's section: what it bounds, then its facts.
fn limit_section(out: &mut String, limit: &Limit) {
    let _ = writeln!(out, "### {}\n", limit.name);
    paragraphs(out, limit.what);

    // The facts, one a line.
    let constant = &limit.constant;
    let _ = writeln!(out, "- Value: {}.", amount(constant.value(), limit.unit));
    let _ = writeln!(
        out,
        "- Constant: `{}`, in `{}`.",
        constant.path, constant.file
    );
    let _ = writeln!(out, "- Commands: {}.", command_links(limit.commands));
    if !limit.errors.is_empty() {
        let _ = writeln!(out, "- Error: {}.", error_links(limit.errors));
    }
    out.push('\n');
}

/// The paging defaults, each with the request field and the flag that set it.
fn defaults_section(out: &mut String) {
    let _ = writeln!(out, "## Defaults\n");
    let _ = writeln!(
        out,
        "What a paged request takes when it does not say. The command line's flag has the same \
         default, and a request over MCP that leaves the field out takes it too.\n"
    );
    let _ = writeln!(
        out,
        "| Command | Flag | Request field | Default | Constant |"
    );
    let _ = writeln!(out, "| --- | --- | --- | --- | --- |");
    for row in &DEFAULTS {
        let flag = row
            .flag
            .map_or_else(|| "none".to_owned(), |flag| format!("`--{flag}`"));
        let _ = writeln!(
            out,
            "| {} | {flag} | [`{}`]({SCHEMA_TYPES}#{}) `{}` | {} | `{}` |",
            command_links(&[row.command]),
            row.request,
            slug(row.request),
            row.field,
            amount(row.constant.value(), row.unit),
            row.constant.path
        );
    }
    out.push('\n');
}

/// Every bound a request type's JSON Schema states: a minimum above 0, a maximum, a length, a
/// pattern.
fn schema_bounds(out: &mut String) {
    let _ = writeln!(out, "## Bounds in the JSON Schema\n");
    let _ = writeln!(
        out,
        "The bounds `swarmops schema` states on a field, beside the integer formats' own. A \
         request that breaks one is `E_INVALID_ARG`.\n"
    );
    let _ = writeln!(out, "| Type | Field | Bound |");
    let _ = writeln!(out, "| --- | --- | --- |");
    for (name, field, bound) in stated_bounds() {
        let _ = writeln!(
            out,
            "| [`{name}`]({SCHEMA_TYPES}#{}) | `{field}` | {} |",
            slug(&name),
            cell(&bound)
        );
    }
    out.push('\n');
}

/// The bounds the schema states, by type and field in name order: `(type, field, bound)`.
fn stated_bounds() -> Vec<(String, String, String)> {
    let all = schema_for_all();
    let mut rows = Vec::new();
    let Some(types) = all.as_object() else {
        return rows;
    };
    for (name, schema) in types {
        let Some(properties) = schema["properties"].as_object() else {
            continue;
        };
        for (field, property) in properties {
            let bound = schema_bound(property);
            if bound.is_empty() {
                continue;
            }
            rows.push((name.clone(), field.clone(), bound));
        }
    }
    rows
}

/// A field's stated bound as a phrase, or nothing: the minimum 0 of an unsigned format is the
/// format's, not a bound.
fn schema_bound(property: &Value) -> String {
    let mut parts = Vec::new();

    // 1. A range of numbers.
    let minimum = property["minimum"].as_u64().filter(|min| *min > 0);
    let maximum = property["maximum"].as_u64();
    match (minimum, maximum) {
        (Some(min), Some(max)) => parts.push(format!("{min} to {max}")),
        (Some(min), None) => parts.push(format!("at least {min}")),
        (None, Some(max)) => parts.push(format!("at most {max}")),
        (None, None) => {}
    }

    // 2. A string's length and grammar.
    if let Some(max) = property["maxLength"].as_u64() {
        parts.push(format!("at most {max} characters"));
    }
    if let Some(pattern) = property["pattern"].as_str() {
        parts.push(format!("matches `{pattern}`"));
    }
    parts.join("; ")
}

/// The internal values, apart from the contract.
fn internal_section(out: &mut String) {
    let _ = writeln!(out, "## Internal values\n");
    let _ = writeln!(
        out,
        "Not part of the contract: these tune the implementation, or bound the store's layout \
         where no request reaches. No option sets them; they are listed for operators, and a \
         release can change them.\n"
    );
    let _ = writeln!(out, "| Value | Amount | Constant | What it does |");
    let _ = writeln!(out, "| --- | --- | --- | --- |");
    for value in &INTERNAL {
        let _ = writeln!(
            out,
            "| {} | {} | `{}` | {} |",
            value.name,
            amount(value.constant.value(), value.unit),
            value.constant.path,
            cell(value.what)
        );
    }
    out.push('\n');
}

/// The hand-written note, under `## Details`, its links a folder higher.
fn details(out: &mut String) {
    let Ok(note) = fs::read_to_string(reference_dir().join(NOTE)) else {
        return;
    };
    let _ = writeln!(out, "## Details\n");
    let _ = writeln!(out, "{}\n", relink(note.trim()));
}

/// A note's text as the page shows it, a folder higher: `](../` becomes `](`.
fn relink(note: &str) -> String {
    note.replace("](../", "](")
}

// ---- Words ----

/// A value with its unit: `67,108,864 bytes (64 MiB)`, `64 levels`, `10 s`.
fn amount(value: u64, unit: Unit) -> String {
    let number = grouped(value);
    match unit {
        Unit::Bytes => match binary(value) {
            Some(short) => format!("{number} bytes ({short})"),
            None => format!("{number} bytes"),
        },
        Unit::Seconds => format!("{number} s"),
        Unit::Millis => format!("{number} ms"),
        Unit::Count(things) => format!("{number} {things}"),
    }
}

/// A byte count in the largest binary unit that divides it, from KiB: `64 MiB`.
fn binary(value: u64) -> Option<String> {
    let units = [("GiB", KIB * KIB * KIB), ("MiB", KIB * KIB), ("KiB", KIB)];
    units
        .iter()
        .find(|(_, size)| value >= *size && value.is_multiple_of(*size))
        .map(|(name, size)| format!("{} {name}", value / size))
}

/// A number with a comma between each three digits: `65,535`.
fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        let left = digits.len() - i;
        if i > 0 && left.is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// Each command linked to its page: `[`commit`](commands/commit.md)`.
fn command_links(commands: &[&str]) -> String {
    let links: Vec<String> = commands
        .iter()
        .map(|command| {
            let path: Vec<String> = command.split(' ').map(str::to_owned).collect();
            format!("[`{command}`]({}/{})", command_page::DIR, file_name(&path))
        })
        .collect();
    links.join(", ")
}

/// Each error code linked to its section of `errors.md`.
fn error_links(errors: &[ErrorCode]) -> String {
    let links: Vec<String> = errors
        .iter()
        .map(|code| format!("[`{0}`](errors.md#{1})", code.as_str(), slug(code.as_str())))
        .collect();
    links.join(", ")
}

// ---- Tests ----

/// The value of a declaration's right-hand side: an integer (`65_535`), two joined by `*` or
/// `<<`, or `Duration::from_secs(n)` / `Duration::from_millis(n)` by its `n`. `None` for
/// anything else.
fn evaluate(decl: &str) -> Option<u64> {
    let (_, rhs) = decl.split_once(" = ")?;
    let rhs = rhs.strip_suffix(';')?.trim();

    // 1. A duration, by its number.
    let duration = ["Duration::from_secs(", "Duration::from_millis("]
        .iter()
        .find_map(|call| rhs.strip_prefix(call)?.strip_suffix(')'));
    if let Some(n) = duration {
        return integer(n);
    }

    // 2. Two integers joined by an operator, or one.
    if let Some((a, b)) = rhs.split_once(" << ") {
        return integer(a)?.checked_shl(u32::try_from(integer(b)?).ok()?);
    }
    if let Some((a, b)) = rhs.split_once(" * ") {
        return integer(a)?.checked_mul(integer(b)?);
    }
    integer(rhs)
}

/// An integer literal, `_` between its digits allowed.
fn integer(text: &str) -> Option<u64> {
    text.trim().replace('_', "").parse().ok()
}

/// Every constant of the page, by the name of its row.
fn constants() -> Vec<(String, &'static Constant)> {
    let limits = CONTRACT.iter().chain(&BOUNDS).chain(&INTERNAL);
    let mut out: Vec<(String, &'static Constant)> = limits
        .map(|limit| (limit.name.to_owned(), &limit.constant))
        .collect();
    for row in &DEFAULTS {
        out.push((format!("{} {}", row.request, row.field), &row.constant));
    }
    out
}

/// Each constant is declared as the page says, in the file it names, and a private one's value
/// is its declaration's.
#[test]
fn values_match_the_code() {
    let root = reference_dir().join("../..");
    for (name, constant) in constants() {
        // 1. The declaration is in its file, and names the constant the path does.
        let text = fs::read_to_string(root.join(constant.file))
            .unwrap_or_else(|e| panic!("{name}: {}: {e}", constant.file));
        assert!(
            text.contains(constant.decl),
            "{name}: {} does not declare `{}`",
            constant.file,
            constant.decl
        );
        let item = constant.path.rsplit("::").next().unwrap();
        assert!(
            constant.decl.contains(&format!("const {item}:")),
            "{name}: `{}` declares no {item}",
            constant.decl
        );

        // 2. A private constant's value is what its declaration gives.
        if constant.origin == Origin::Declared {
            assert_eq!(
                evaluate(constant.decl),
                Some(constant.value()),
                "{name}: `{}`",
                constant.decl
            );
        }
    }
}

/// The limits that public functions check hold at their values: 255 bytes pass, 256 fail.
#[test]
fn checked_limits_hold() {
    let name = |len: usize| format!("a/{}", "b".repeat(len - 2));
    assert!(swarmops_api::Identity::parse(&name(255)).is_ok());
    assert!(swarmops_api::Identity::parse(&name(256)).is_err());

    let release = |len: usize| format!("release/{}", "x".repeat(len - "release/".len()));
    assert!(swarmops_store::check_ref_name(&release(255)).is_ok());
    assert!(swarmops_store::check_ref_name(&release(256)).is_err());
}

/// Each command a row names exists, and each default is the command's flag default and the
/// schema's.
#[test]
fn defaults_and_commands_match() {
    let leaves = all_leaves();
    let find = |command: &str| {
        leaves
            .iter()
            .find(|leaf| leaf.path.join(" ") == command)
            .unwrap_or_else(|| panic!("{command}: no such command"))
    };

    // 1. Every named command exists.
    let limits = CONTRACT.iter().chain(&BOUNDS);
    for limit in limits {
        for command in limit.commands {
            find(command);
        }
    }

    // 2. A default is the flag's and the schema's.
    let all = schema_for_all();
    for row in &DEFAULTS {
        let field = &all[row.request]["properties"][row.field];
        assert_eq!(
            field["default"].as_u64(),
            Some(row.constant.value()),
            "{} {}: the schema's default",
            row.request,
            row.field
        );
        let leaf = find(row.command);
        let Some(flag) = row.flag else {
            continue;
        };
        let arg = leaf
            .cmd
            .get_arguments()
            .find(|arg| arg.get_long() == Some(flag))
            .unwrap_or_else(|| panic!("{}: no --{flag}", row.command));
        let default: Vec<String> = arg
            .get_default_values()
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            default,
            [row.constant.value().to_string()],
            "{} --{flag}",
            row.command
        );
    }
}

/// The agent name's commands are `whoami`, `mcp` and every command whose page says it needs an
/// identity.
#[test]
fn identity_commands_match() {
    let agent = CONTRACT
        .iter()
        .find(|limit| limit.name == "Agent name")
        .unwrap();
    let mut want: Vec<String> = vec!["whoami".to_owned(), "mcp".to_owned()];
    for leaf in all_leaves() {
        let page = reference_dir()
            .join(command_page::DIR)
            .join(file_name(&leaf.path));
        let text = fs::read_to_string(&page).unwrap_or_default();
        if text.contains("\nidentity_required: true\n") {
            want.push(leaf.path.join(" "));
        }
    }
    let mut got: Vec<String> = agent.commands.iter().map(|c| (*c).to_owned()).collect();
    want.sort();
    got.sort();
    assert_eq!(got, want);
}

/// Numbers and declarations read as the page needs.
#[test]
fn words_are_sound() {
    assert_eq!(grouped(67_108_864), "67,108,864");
    assert_eq!(grouped(999), "999");
    assert_eq!(amount(64 << 20, Unit::Bytes), "67,108,864 bytes (64 MiB)");
    assert_eq!(amount(255, Unit::Bytes), "255 bytes");
    assert_eq!(evaluate("const A: u64 = 24 * 1024;"), Some(24_576));
    assert_eq!(evaluate("const A: u64 = 16 << 20;"), Some(16 << 20));
    assert_eq!(
        evaluate("const A: Duration = Duration::from_secs(10);"),
        Some(10)
    );
    assert_eq!(evaluate("const A: usize = 8_000;"), Some(8000));
    assert_eq!(
        relink("[a](../errors.md) [b](#b)"),
        "[a](errors.md) [b](#b)"
    );
}
