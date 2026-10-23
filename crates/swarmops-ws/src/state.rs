//! `workspace.json` (§2.2 of the M5 design): the workspace's durable state, in its `.swarmws/`.
//! It names the agent's committed head, so it is replaced whole and durably
//! (`FsOps::atomic_replace`: temp, sync, rename, directory sync); a stale `head` after a power
//! cut would show the agent's commits as edits.
//!
//! The M5 plan's task W1 froze the type and its read and replace; task W5 writes the state
//! transitions.

use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use swarmops_api::{ErrorCode, OpError};
use swarmops_store::{FsMethod, FsOps};

use crate::io_error;

/// The workspace's private directory, at its root; never a tree path.
pub const SWARMWS_DIR: &str = ".swarmws";

/// The state file, in [`SWARMWS_DIR`].
pub const WORKSPACE_JSON: &str = "workspace.json";

/// The format of a `workspace.json` this build reads and writes.
pub const WORKSPACE_FORMAT: u32 = 1;

/// A workspace's durable state.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceFile {
    /// The file's format: [`WORKSPACE_FORMAT`].
    pub format: u32,
    /// The workspace's name, its registry entry's.
    pub name: String,
    /// The store's root on the host, as the caller named it.
    pub repo: String,
    /// The store's id: a workspace of another store is refused.
    pub repo_id: String,
    /// The target's changeset the chain starts from.
    pub base: String,
    /// The chain's last changeset; `base` when nothing is committed. Local edits are against
    /// its tree.
    pub head: String,
    /// The repository paths whose subtrees are written out; paths outside are absent.
    pub prefixes: Vec<String>,
    /// The whole tree, instead of `prefixes`.
    pub all: bool,
    /// The identity that made the workspace, which a mutation without one acts as.
    pub created_by: String,
    /// When it was made, in Unix milliseconds.
    pub created_at: u64,
    /// The ref the workspace lands on: `main`, or `release/<name>`.
    pub target: String,
    /// Clean, or rebasing.
    pub state: State,
    /// The target's head the last rebase that succeeded merged onto; cleared by a commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onto: Option<String>,
    /// What the volume holds, probed at creation.
    pub volume: Volume,
    /// The changeset whose tree the files stand for: `head` after a commit, the rebased
    /// changeset after a rebase, the landed one after a land.
    pub materialized: String,
}

impl WorkspaceFile {
    /// The state of workspace `name` in [`WORKSPACE_FORMAT`], which a read checks again; no
    /// `onto`. The fields are those of the struct, in its order.
    #[allow(
        clippy::too_many_arguments,
        reason = "every field but `onto` is required; a `Default` would need a made-up state"
    )]
    pub fn new(
        name: String,
        repo: String,
        repo_id: String,
        base: String,
        head: String,
        prefixes: Vec<String>,
        all: bool,
        created_by: String,
        created_at: u64,
        target: String,
        state: State,
        volume: Volume,
        materialized: String,
    ) -> WorkspaceFile {
        WorkspaceFile {
            format: WORKSPACE_FORMAT,
            name,
            repo,
            repo_id,
            base,
            head,
            prefixes,
            all,
            created_by,
            created_at,
            target,
            state,
            onto: None,
            volume,
            materialized,
        }
    }
}

/// A workspace's state: clean, or stopped in a rebase on conflicts.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum State {
    /// No rebase in progress. A land in flight is the pending record, not a state.
    Clean,
    /// A rebase stopped on conflicts.
    Rebasing {
        /// The target's head the rebase merges onto.
        onto: String,
        /// The conflict report's `theirs`, which the resolutions are decided against.
        against: String,
    },
}

/// What the workspace's volume holds, probed once at creation.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "three independent facts of the volume, as workspace.json spells them"
)]
pub struct Volume {
    /// Two names that differ only in case name one file.
    pub case_insensitive: bool,
    /// A file keeps its executable bit.
    pub exec_bit: bool,
    /// A symlink can be made.
    pub symlinks: bool,
}

impl Volume {
    /// A volume that holds what the three facts say: `const`, so that a test can name one.
    pub const fn new(case_insensitive: bool, exec_bit: bool, symlinks: bool) -> Volume {
        Volume {
            case_insensitive,
            exec_bit,
            symlinks,
        }
    }
}

/// The path of `workspace.json` in the workspace at `dir`.
pub fn path_in(dir: &Utf8Path) -> Utf8PathBuf {
    dir.join(SWARMWS_DIR).join(WORKSPACE_JSON)
}

/// The `workspace.json` of the workspace at `dir`. `E_WS_NOT_FOUND` when there is none,
/// `E_INVALID_ARG` for one that does not parse or that a newer build wrote.
pub fn read(fs: &dyn FsOps, dir: &Utf8Path) -> Result<WorkspaceFile, OpError> {
    // 1. The bytes, if the file is there.
    let file = relative();
    let Some(bytes) = (fs.read(path_in(dir).as_std_path()))
        .map_err(|e| io_error::io_failed(FsMethod::Read, &file, &e))?
    else {
        return Err(OpError::new(
            ErrorCode::WsNotFound,
            format!("no workspace at {dir}: it has no {file}"),
        ));
    };

    // 2. The format first, so that a newer one is named rather than misread.
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| unreadable(&file, &e))?;
    let format = value.get("format").and_then(Value::as_u64);
    if format != Some(u64::from(WORKSPACE_FORMAT)) {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            format!(
                "{file} has format {}, which this build does not read (it reads \
                 {WORKSPACE_FORMAT}): a newer swarmops wrote it",
                value.get("format").unwrap_or(&Value::Null)
            ),
        )
        .with_details(Map::from_iter([
            ("file".to_owned(), Value::from(file.as_str())),
            ("format".to_owned(), value["format"].clone()),
        ])));
    }

    // 3. Then the state itself.
    serde_json::from_value(value).map_err(|e| unreadable(&file, &e))
}

/// Replaces the `workspace.json` of the workspace at `dir` with `file`, durably. Its
/// `.swarmws/` exists.
pub fn replace(fs: &dyn FsOps, dir: &Utf8Path, file: &WorkspaceFile) -> Result<(), OpError> {
    let bytes = serde_json::to_vec(file).expect("a workspace's state serializes");
    (fs.atomic_replace(path_in(dir).as_std_path(), &bytes))
        .map_err(|e| io_error::io_failed(FsMethod::AtomicReplace, &relative(), &e))
}

/// `workspace.json`'s path below the workspace's root, as errors name it.
fn relative() -> String {
    format!("{SWARMWS_DIR}/{WORKSPACE_JSON}")
}

/// `E_INVALID_ARG` for a `workspace.json` that does not parse.
fn unreadable(file: &str, e: &serde_json::Error) -> OpError {
    OpError::new(
        ErrorCode::InvalidArg,
        format!("{file} is not a workspace's state: {e}"),
    )
    .with_details(Map::from_iter([("file".to_owned(), Value::from(file))]))
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::{Value, json};
    use swarmops_store::RealFs;

    use super::*;

    /// A workspace as `ws create` records one.
    pub(crate) fn sample(name: &str, state: State) -> WorkspaceFile {
        let id = |c: char| c.to_string().repeat(64);
        WorkspaceFile {
            format: WORKSPACE_FORMAT,
            name: name.to_owned(),
            repo: "/stores/acme".to_owned(),
            repo_id: "0".repeat(32),
            base: id('b'),
            head: id('c'),
            prefixes: vec!["src".to_owned(), "docs".to_owned()],
            all: false,
            created_by: format!("claude-code/{name}"),
            created_at: 1_792_630_000_000,
            target: "main".to_owned(),
            state,
            onto: None,
            volume: Volume {
                case_insensitive: false,
                exec_bit: true,
                symlinks: true,
            },
            materialized: id('c'),
        }
    }

    /// `workspace.json` is written whole and read back equal, in its wire form (the state a
    /// tagged object, `onto` omitted when none); a format this build does not know is refused,
    /// and so is a file that does not parse; a workspace without one is not found.
    #[test]
    fn workspace_json_round_trips_and_rejects_an_unknown_format() {
        // 1. A workspace directory with its .swarmws/.
        let tmp = tempfile::tempdir().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();
        std::fs::create_dir(dir.join(SWARMWS_DIR)).unwrap();
        let fs = RealFs;

        // 2. A clean workspace, then a rebasing one, each read back as written.
        let clean = sample("worker-3", State::Clean);
        replace(&fs, dir, &clean).unwrap();
        assert_eq!(read(&fs, dir).unwrap(), clean);
        let text = std::fs::read_to_string(path_in(dir)).unwrap();
        let wire: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(wire["format"], 1);
        assert_eq!(wire["state"], json!({"kind": "clean"}));
        assert!(wire.get("onto").is_none(), "{wire}");
        let rebasing = WorkspaceFile {
            onto: Some("d".repeat(64)),
            state: State::Rebasing {
                onto: "d".repeat(64),
                against: "e".repeat(64),
            },
            ..clean.clone()
        };
        replace(&fs, dir, &rebasing).unwrap();
        assert_eq!(read(&fs, dir).unwrap(), rebasing);

        // 3. A newer format is refused, naming it.
        let mut newer = serde_json::to_value(&clean).unwrap();
        newer["format"] = json!(2);
        std::fs::write(path_in(dir), newer.to_string()).unwrap();
        let refused = read(&fs, dir).unwrap_err();
        assert_eq!(refused.code, "E_INVALID_ARG", "{refused:?}");
        assert_eq!(refused.details["format"], 2);

        // 4. So is a file that does not parse.
        std::fs::write(path_in(dir), b"{\"format\": 1").unwrap();
        assert_eq!(read(&fs, dir).unwrap_err().code, "E_INVALID_ARG");

        // 5. No workspace.json: no workspace there.
        std::fs::remove_file(path_in(dir)).unwrap();
        assert_eq!(read(&fs, dir).unwrap_err().code, "E_WS_NOT_FOUND");
    }
}
