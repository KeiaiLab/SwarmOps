//! The Claude Code adapter (§7 of the M5 design): the two commands a settings file registers as
//! the `WorktreeCreate` and `WorktreeRemove` hooks. Create makes a workspace for the session and
//! gives the path Claude Code prints; remove keeps uncommitted edits as a rescue changeset, then
//! deletes the workspace.
//!
//! The parsers take the hook's stdin bytes, at most [`MAX_HOOK_INPUT`]; the two hook functions
//! work against a [`Ctx`] the CLI built, and read no environment variable: the CLI hands what it
//! read as arguments ([`Bootstrap`], the workspaces' root).
//!
//! ```text
//! worktree-create {name, session_id, cwd}
//!   cwd inside a workspace? its head and prefixes : main and the repository's prefixes
//!   name = slug(name), + "-<7 hex>" when another session (or none) holds it
//!   create <ws_root>/<repo>/<name> as claude-code/<name> ─▶ bootstrap when new ─▶ the path
//! worktree-remove {worktree_path}
//!   edits ─▶ rescue changeset on head ─▶ the directory, then the entry ─▶ {head, rescued}
//! ```
//!
//! [`opencode`] is the second host's family (the M19 design §9.2): the same bodies, called
//! with what that host gives.

use std::path::Path;
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use swarmops_api::{
    ErrorCode, Identity, InfoReq, OpError, StatusAfter, WsCreateReq, WsDeleteReq, WsInfo,
};
use swarmops_core::portable;
use swarmops_core::{ChangesetId, ObjectId};
use swarmops_store::FsOps;

use crate::registry::{Entry, Registry};
use crate::scan::Want;
use crate::workspace::{self, Found, Locate, Workspace};
use crate::{Ctx, bootstrap, create, delete, materialize, status};

/// The most bytes of hook input the parsers read.
pub const MAX_HOOK_INPUT: usize = 64 * 1024;

/// The `WorktreeCreate` hook's input: the fields a workspace is made from.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HookCreate {
    /// The slug Claude Code suggests.
    pub name: String,
    /// The session that asks.
    pub session_id: String,
    /// Where the session stands.
    pub cwd: Utf8PathBuf,
}

impl HookCreate {
    /// The input of session `session_id`, standing in `cwd`, that asks for a workspace `name`.
    pub fn new(name: String, session_id: String, cwd: Utf8PathBuf) -> HookCreate {
        HookCreate {
            name,
            session_id,
            cwd,
        }
    }
}

/// The `WorktreeRemove` hook's input.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HookRemove {
    /// The directory to remove: the path the create hook printed.
    pub worktree_path: Utf8PathBuf,
}

impl HookRemove {
    /// The input that removes the workspace at `worktree_path`.
    pub fn new(worktree_path: Utf8PathBuf) -> HookRemove {
        HookRemove { worktree_path }
    }
}

/// Whether a new workspace's bootstrap runs: `SWARMOPS_WS_BOOTSTRAP=skip`, as the CLI read it,
/// is [`Bootstrap::Skip`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Bootstrap {
    /// Copy the `.worktreeinclude` files and run the repository's bootstrap command.
    Run,
    /// Neither.
    Skip,
}

/// What the remove hook left in the store: the workspace's head, and the rescue changeset of its
/// uncommitted edits, when it had some.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct RemoveReport {
    /// The workspace's head when it was removed.
    pub head: ChangesetId,
    /// The rescue changeset, on `head`.
    pub rescued: Option<ChangesetId>,
}

/// The ref a workspace made outside any other starts from.
const DEFAULT_BASE: &str = "main";

/// The identity family of a workspace the hook makes without the caller's identity.
const HOOK_FAMILY: &str = "claude-code";

/// The prefix of a slug that would be no workspace name (empty, a leading dot, a name a
/// platform cannot hold).
const NAME_PREFIX: &str = "ws-";

/// The most bytes of a slug: a suffix of [`SUFFIX_LEN`] hex digits and its dash still fit the
/// 64 of a workspace name.
const MAX_SLUG: usize = 56;

/// The hex digits of the suffix of a name another session holds.
const SUFFIX_LEN: usize = 7;

/// The hex digits of the session's digest an entry records.
const SESSION_LEN: usize = 16;

/// The hex digits of the store's id that name its directory when its name gives none.
const REPO_ID_LEN: usize = 12;

/// The command the two settings entries run.
const HOOK_COMMAND: &str = "swarmops ws hook";

/// Who sends the Claude Code hooks' input, as a parse error names it.
const CLAUDE_CODE: &str = "Claude Code";

/// Where a new workspace starts: a base and the paths it holds.
struct Start {
    /// The ref or changeset it starts from.
    base: String,
    /// The prefixes it holds; none with `all`.
    prefixes: Vec<String>,
    /// Whether it holds the whole tree.
    all: bool,
}

/// What a workspace a hook makes is: its name, directory and creator, where it starts, the
/// directory its bootstrap copies from, and the request it answers (an entry's `session`).
struct Making<'a> {
    /// The workspace's name.
    name: String,
    /// Its directory.
    dir: Utf8PathBuf,
    /// Who makes it.
    who: Identity,
    /// Where it starts.
    start: Start,
    /// The directory its bootstrap copies from.
    from: &'a Utf8Path,
    /// The request's digest, which makes the same request again the same workspace.
    session: String,
}

/// What becomes of a workspace whose bootstrap failed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Failed {
    /// Kept for inspection.
    Keep,
    /// Deleted: it holds no edit yet.
    Remove,
}

/// Whose workspaces a remove takes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Owner {
    /// Any workspace of the store.
    Any,
    /// Only those of the identity family named.
    Family(&'static str),
}

/// The `WorktreeCreate` input's fields the hook reads; the others are passed over.
#[derive(Deserialize)]
struct CreateInput {
    name: String,
    session_id: String,
    cwd: String,
}

/// The `WorktreeRemove` input's field the hook reads.
#[derive(Deserialize)]
struct RemoveInput {
    worktree_path: String,
}

/// The `WorktreeCreate` hook's input, from its stdin bytes: at most [`MAX_HOOK_INPUT`], with
/// `name`, `session_id` and an absolute `cwd`; else `E_INVALID_ARG`.
pub fn read_create(stdin: &[u8]) -> Result<HookCreate, OpError> {
    let input: CreateInput = parse(stdin, "WorktreeCreate", CLAUDE_CODE)?;
    Ok(HookCreate {
        name: input.name,
        session_id: input.session_id,
        cwd: workspace::host_path(Path::new(&input.cwd))?,
    })
}

/// The `WorktreeRemove` hook's input, from its stdin bytes: at most [`MAX_HOOK_INPUT`], with an
/// absolute `worktree_path`; else `E_INVALID_ARG`.
pub fn read_remove(stdin: &[u8]) -> Result<HookRemove, OpError> {
    let input: RemoveInput = parse(stdin, "WorktreeRemove", CLAUDE_CODE)?;
    Ok(HookRemove {
        worktree_path: workspace::host_path(Path::new(&input.worktree_path))?,
    })
}

/// Makes the session's workspace under `ws_root` and runs its bootstrap as `bootstrap` says:
/// the absolute path the CLI prints. The same session asking for the same name again gets the
/// same workspace, not bootstrapped again.
pub fn worktree_create(
    ctx: &Ctx<'_>,
    input: &HookCreate,
    ws_root: &Utf8Path,
    bootstrap: Bootstrap,
) -> Result<Utf8PathBuf, OpError> {
    // 1. Where it starts: from the workspace the session stands in, as a git worktree starts
    //    from HEAD; else from main, holding the repository's prefixes or the whole tree.
    let start = start_of(ctx, &input.cwd, None)?;

    // 2. Its name: the slug, suffixed when another session, or a hand-made workspace, holds it.
    let session = digest(&[input.session_id.as_bytes()], SESSION_LEN);
    let suffix = digest(
        &[input.session_id.as_bytes(), input.name.as_bytes()],
        SUFFIX_LEN,
    );
    let name = free_name(ctx, slug(&input.name), &session, &suffix)?;

    // 3. Where, and as whom: `<ws_root>/<repo>/<name>`, the caller or `claude-code/<name>`.
    let info = ctx.store.repo_info(InfoReq::default())?;
    let dir = ws_root
        .join(repo_dir(&info.name, &info.repo_id))
        .join(&name);
    let who = match &ctx.who {
        Some(who) => who.clone(),
        None => family_member(HOOK_FAMILY, &name)?,
    };

    // 4. Made, then bootstrapped when this create made it.
    let making = Making {
        name,
        dir: dir.clone(),
        who,
        start,
        from: &input.cwd,
        session,
    };
    make(ctx, making, bootstrap, Failed::Keep)?;
    Ok(dir)
}

/// The workspace at the directory the remove hook names, if there is one: none is a directory
/// that is not ours, which the CLI leaves alone and exits 0 on. No store is needed.
pub fn ours(input: &HookRemove, fs: &dyn FsOps) -> Result<Option<Found>, OpError> {
    match workspace::find(&Locate::Path(input.worktree_path.clone()), None, fs) {
        Ok(found) => Ok(Some(found)),
        Err(e) if e.code == ErrorCode::WsNotFound.as_str() => Ok(None),
        Err(e) => Err(e),
    }
}

/// Keeps the workspace's uncommitted edits as a rescue changeset on its head, then deletes it:
/// the directory, then its entry. `E_WS_NOT_FOUND` for a directory that is no workspace.
pub fn worktree_remove(ctx: &Ctx<'_>, input: &HookRemove) -> Result<RemoveReport, OpError> {
    take_down(ctx, &input.worktree_path, Owner::Any, opencode::Force::On)
}

/// The JSON of the two hook entries, for the `hooks` of a Claude Code settings file.
pub fn config() -> String {
    let entry = |verb: &str| {
        json!([{
            "hooks": [{"type": "command", "command": format!("{HOOK_COMMAND} {verb}")}]
        }])
    };
    let settings = json!({
        "hooks": {
            "WorktreeCreate": entry("worktree-create"),
            "WorktreeRemove": entry("worktree-remove"),
        }
    });
    let mut text = serde_json::to_string_pretty(&settings).expect("the settings serialize");
    text.push('\n');
    text
}

/// The hook input `stdin` of `event`, which `host` sends, parsed: `E_INVALID_ARG` past
/// [`MAX_HOOK_INPUT`] bytes, for input that is not JSON, or one without a field the hook reads.
fn parse<T: DeserializeOwned>(stdin: &[u8], event: &str, host: &str) -> Result<T, OpError> {
    if stdin.len() > MAX_HOOK_INPUT {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            format!(
                "the {event} hook's input holds {} bytes, past the {MAX_HOOK_INPUT} read",
                stdin.len()
            ),
        )
        .with_details(Map::from_iter([
            ("size".to_owned(), Value::from(stdin.len())),
            ("max".to_owned(), Value::from(MAX_HOOK_INPUT)),
        ])));
    }
    serde_json::from_slice(stdin).map_err(|e| {
        OpError::new(
            ErrorCode::InvalidArg,
            format!("the {event} hook's input is not what {host} sends: {e}"),
        )
    })
}

/// Where a workspace made from `cwd` starts: the workspace there (its head, or `start` when
/// given, and its prefixes); else `start` or `main`, holding the repository's prefixes there or
/// the whole tree.
fn start_of(ctx: &Ctx<'_>, cwd: &Utf8Path, start: Option<&str>) -> Result<Start, OpError> {
    // 1. Inside a workspace: its prefixes.
    if let Some(parent) = parent_of(ctx, cwd)? {
        let base = start.map_or(parent.file.head, str::to_owned);
        return Ok(Start {
            base,
            prefixes: parent.file.prefixes,
            all: parent.file.all,
        });
    }

    // 2. Outside: the repository's prefixes at the base, none being the whole tree.
    let base = start.unwrap_or(DEFAULT_BASE);
    let config = bootstrap::repo_config(ctx.store, base)?.unwrap_or_default();
    Ok(Start {
        base: base.to_owned(),
        all: config.prefixes.is_empty(),
        prefixes: config.prefixes,
    })
}

/// `name`, or `name-<suffix>` when another request (another `session`, or none) holds it.
fn free_name(ctx: &Ctx<'_>, name: String, session: &str, suffix: &str) -> Result<String, OpError> {
    let registry = Registry::open(&ctx.store_root, Arc::clone(&ctx.fs));
    let taken = registry.read(&name)?;
    if taken.is_some_and(|entry| entry.session.as_deref() != Some(session)) {
        return Ok(format!("{name}-{suffix}"));
    }
    Ok(name)
}

/// The identity `<family>/<name>` of a workspace a hook makes for no caller.
fn family_member(family: &str, name: &str) -> Result<Identity, OpError> {
    Identity::parse(&format!("{family}/{name}"))
}

/// Makes the workspace `making` describes, then bootstraps it as `bootstrap` says when this call
/// made it; a failed bootstrap leaves it as `failed` says, and is the call's error.
fn make(
    ctx: &Ctx<'_>,
    making: Making<'_>,
    bootstrap: Bootstrap,
    failed: Failed,
) -> Result<WsInfo, OpError> {
    // 1. The context as its maker.
    let ctx = Ctx {
        store: ctx.store,
        store_root: ctx.store_root.clone(),
        who: Some(making.who),
        fs: Arc::clone(&ctx.fs),
        host: Arc::clone(&ctx.host),
    };

    // 2. Made; the same request again is the workspace there, not bootstrapped again.
    let mut req = WsCreateReq::new(making.name.clone());
    req.path = Some(making.dir.to_string());
    req.base = making.start.base.as_str().into();
    req.prefixes = making.start.prefixes;
    req.all = making.start.all;
    req.from = Some(making.from.to_string());
    req.session = Some(making.session);
    req.status_after = StatusAfter::None;
    let made = create::create(&ctx, req)?;
    if !made.created || bootstrap == Bootstrap::Skip {
        return Ok(made);
    }

    // 3. Bootstrapped; on failure, kept or deleted.
    let ws = Workspace::open(&ctx, Locate::Path(making.dir))?;
    let Err(e) = bootstrap::run(&ctx, &ws, Some(making.from)) else {
        return Ok(made);
    };
    if failed == Failed::Remove {
        let mut req = WsDeleteReq::new(making.name);
        req.force = true;
        delete::delete(&ctx, req)?;
    }
    Err(e)
}

/// Keeps the uncommitted edits of the workspace at `dir` as a rescue changeset on its head,
/// then deletes it: the directory, then its entry. `owner` says whose it may be; without
/// `force`, edits are `E_WS_DIRTY` (`details.force: true`) and nothing changes.
fn take_down(
    ctx: &Ctx<'_>,
    dir: &Utf8Path,
    owner: Owner,
    force: opencode::Force,
) -> Result<RemoveReport, OpError> {
    // 1. The workspace there, and its entry pointing at it: the delete goes by name.
    let ws = Workspace::open(ctx, Locate::Path(dir.to_owned()))?;
    let name = ws.file.name.clone();
    let registry = Registry::open(&ctx.store_root, Arc::clone(&ctx.fs));
    let entry = registry.read(&name)?;
    let Some(entry) = entry.filter(|entry| entry.path == ws.dir.as_str()) else {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            format!(
                "the store's workspace {name:?} is not the one at {}",
                ws.dir
            ),
        )
        .with_details(Map::from_iter([("name".to_owned(), Value::from(name))]))
        .with_hint("swarmops ws list --check"));
    };

    // 2. Only the owner's: what a family did not make, it does not delete.
    if let Owner::Family(family) = owner
        && !of_family(&entry, family)
    {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            format!(
                "workspace {name:?} was made by {}, not by the {family} hooks: left as it is",
                entry.created_by
            ),
        )
        .with_details(Map::from_iter([
            ("name".to_owned(), Value::from(name)),
            ("created_by".to_owned(), Value::from(entry.created_by)),
        ])));
    }

    // 3. Without force, uncommitted edits refuse.
    if force == opencode::Force::Off {
        let want = Want {
            paths: Vec::new(),
            full: false,
        };
        let changed = status::changes_of(ctx, &ws, &want)?;
        if !changed.is_empty() {
            let paths = changed.into_iter().map(|change| change.path).collect();
            let mut refused = materialize::dirty(paths);
            refused
                .details
                .insert("force".to_owned(), Value::Bool(true));
            return Err(refused.with_hint("remove it with force: its edits are kept as a rescue"));
        }
    }

    // 4. Rescued and deleted.
    let mut req = WsDeleteReq::new(name);
    req.rescue = true;
    let done = delete::delete(ctx, req)?;
    let rescued = done.rescued.as_deref().map(changeset_id).transpose()?;
    Ok(RemoveReport {
        head: changeset_id(&done.head)?,
        rescued,
    })
}

/// Whether the workspace of `entry` was made as a member of identity family `family`.
fn of_family(entry: &Entry, family: &str) -> bool {
    Identity::parse(&entry.created_by).is_ok_and(|who| who.family() == family)
}

/// The workspace of `ctx`'s store at or above `cwd`, if any; one of another store is refused
/// (`E_INVALID_ARG`), never passed over.
fn parent_of(ctx: &Ctx<'_>, cwd: &Utf8Path) -> Result<Option<Workspace>, OpError> {
    match Workspace::open(ctx, Locate::Cwd(cwd.to_owned())) {
        Ok(ws) => Ok(Some(ws)),
        Err(e) if e.code == ErrorCode::WsNotFound.as_str() => Ok(None),
        Err(e) => Err(e),
    }
}

/// `text` made a workspace name: lowercased, each character outside `a-z0-9._-` made `-`, cut to
/// [`MAX_SLUG`] bytes, a trailing dot made `-`; prefixed with `ws-` when empty, starting with a
/// dot, or a name a platform cannot hold (`Feature Auth` → `feature-auth`, `.env` → `ws-.env`).
fn slug(text: &str) -> String {
    // 1. The characters of the grammar, cut.
    let allowed = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c);
    let mut name: String = (text.chars())
        .map(|c| c.to_ascii_lowercase())
        .map(|c| if allowed(c) { c } else { '-' })
        .collect();
    name.truncate(MAX_SLUG);

    // 2. No trailing dot, which no platform keeps alike.
    if name.ends_with('.') {
        name.pop();
        name.push('-');
    }

    // 3. The prefix where the slug is no name, cut again.
    if name.is_empty() || name.starts_with('.') || portable::check_component(&name).is_err() {
        name = format!("{NAME_PREFIX}{name}");
        name.truncate(MAX_SLUG);
    }
    name
}

/// The directory of the store named `name`, of id `repo_id`, below the workspaces' root: the
/// name made a slug, or the id's first 12 digits when the name is empty.
fn repo_dir(name: &str, repo_id: &str) -> String {
    if name.is_empty() {
        return repo_id.chars().take(REPO_ID_LEN).collect();
    }
    slug(name)
}

/// The first `len` hex digits of the BLAKE3 hash of `parts`, a zero byte between two (so that
/// two parts never read as one): of one part, its plain hash.
fn digest(parts: &[&[u8]], len: usize) -> String {
    let mut hasher = blake3::Hasher::new();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            hasher.update(&[0]);
        }
        hasher.update(part);
    }
    let mut hex = hasher.finalize().to_hex().to_string();
    hex.truncate(len);
    hex
}

/// The changeset id `hex`, as the store gave it: `E_INTERNAL` for one that is not an id.
fn changeset_id(hex: &str) -> Result<ChangesetId, OpError> {
    ObjectId::from_hex(hex).map(ChangesetId::new).map_err(|e| {
        OpError::new(
            ErrorCode::Internal,
            format!("the store gave {hex:?} as a changeset id: {e}"),
        )
    })
}

/// The second host's family (the M19 design §9.2). The host proposes the directory and calls
/// create, remove and list as commands: one JSON object on stdin, one on stdout.
///
/// ```text
/// create {source, destination, ref?, name?}
///   ref held? else E_REF_NOT_FOUND ─▶ base: the workspace at source | ref | main
///   destination there? the same request's workspace : E_WS_EXISTS
///   name = slug(name | destination's last component), + "-<7 hex>" when another holds it
///   create at destination as opencode/<name> ─▶ bootstrap; failed? the workspace deleted
/// remove {directory, force?}
///   not ours? E_INVALID_ARG ─▶ edits without force? E_WS_DIRTY{force: true}
///   ─▶ rescue changeset on head ─▶ the directory, then the entry ─▶ {head, rescued}
/// list {source} ─▶ [source: root] + the family's workspaces whose directory exists, by path
/// ```
pub mod opencode {
    use std::path::Path;
    use std::sync::Arc;

    use camino::{Utf8Path, Utf8PathBuf};
    use serde::Deserialize;
    use serde_json::{Map, Value};
    use swarmops_api::{ErrorCode, InfoReq, OpError, ResolveRevReq};

    use super::{
        Bootstrap, Failed, Making, Owner, RemoveReport, SESSION_LEN, SUFFIX_LEN, digest,
        family_member, free_name, make, of_family, parse, slug, start_of, take_down,
    };
    use crate::hostfs::HostKind;
    use crate::registry::Registry;
    use crate::{Ctx, materialize, state, workspace};

    /// The identity family of the workspaces this family makes: `opencode/<name>`, whoever
    /// calls, so that its remove and list know them.
    pub const FAMILY: &str = "opencode";

    /// Who sends this family's input, as a parse error names it.
    const HOST: &str = "the host";

    /// The create input: where the agent stands, the directory the host proposes, and the
    /// starting ref and the name when it gave them.
    #[derive(Debug, Clone, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct Create {
        /// Where the agent stands.
        pub source: Utf8PathBuf,
        /// The directory the workspace is made in.
        pub destination: Utf8PathBuf,
        /// The ref it starts from, when given.
        pub start: Option<String>,
        /// The name the host chose, when given.
        pub name: Option<String>,
    }

    impl Create {
        /// The input that makes a workspace at `destination` for an agent standing in
        /// `source`, with no starting ref and no name.
        pub fn new(source: Utf8PathBuf, destination: Utf8PathBuf) -> Create {
            Create {
                source,
                destination,
                start: None,
                name: None,
            }
        }
    }

    /// Whether a remove deletes a workspace that holds uncommitted edits (keeping them as a
    /// rescue changeset first).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    #[non_exhaustive]
    pub enum Force {
        /// Refuse it.
        Off,
        /// Delete it.
        On,
    }

    /// The remove input.
    #[derive(Debug, Clone, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct Remove {
        /// The workspace's directory.
        pub directory: Utf8PathBuf,
        /// Whether edits are no obstacle.
        pub force: Force,
    }

    impl Remove {
        /// The input that removes the workspace at `directory`, as `force` says.
        pub fn new(directory: Utf8PathBuf, force: Force) -> Remove {
            Remove { directory, force }
        }
    }

    /// What create answers: the workspace's directory, name and head.
    #[derive(Debug, Clone, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct Made {
        /// Its directory, absolute: the destination.
        pub path: Utf8PathBuf,
        /// Its name in the store's registry.
        pub name: String,
        /// Its head changeset.
        pub head: String,
    }

    /// What a listed directory is.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    #[non_exhaustive]
    pub enum Kind {
        /// The source directory itself.
        Root,
        /// A workspace of this family.
        Workspace,
    }

    /// One entry of the list.
    #[derive(Debug, Clone, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct Shown {
        /// The directory, absolute.
        pub path: Utf8PathBuf,
        /// What it is.
        pub kind: Kind,
    }

    /// The create input's fields; the others are passed over.
    #[derive(Deserialize)]
    struct CreateInput {
        source: String,
        destination: String,
        #[serde(default, rename = "ref")]
        start: Option<String>,
        #[serde(default)]
        name: Option<String>,
    }

    /// The remove input's fields.
    #[derive(Deserialize)]
    struct RemoveInput {
        directory: String,
        #[serde(default)]
        force: bool,
    }

    /// The list input's field.
    #[derive(Deserialize)]
    struct ListInput {
        source: String,
    }

    /// The create input, from its stdin bytes: at most [`MAX_HOOK_INPUT`](super::MAX_HOOK_INPUT), with an absolute
    /// `source` and `destination`; an empty `ref` or `name` is none. Else `E_INVALID_ARG`.
    pub fn read_create(stdin: &[u8]) -> Result<Create, OpError> {
        let input: CreateInput = parse(stdin, "create", HOST)?;
        let given = |text: Option<String>| text.filter(|text| !text.is_empty());
        Ok(Create {
            source: workspace::host_path(Path::new(&input.source))?,
            destination: workspace::host_path(Path::new(&input.destination))?,
            start: given(input.start),
            name: given(input.name),
        })
    }

    /// The remove input, from its stdin bytes: an absolute `directory`, `force` true or false
    /// (false when absent). Else `E_INVALID_ARG`.
    pub fn read_remove(stdin: &[u8]) -> Result<Remove, OpError> {
        let input: RemoveInput = parse(stdin, "remove", HOST)?;
        let force = if input.force { Force::On } else { Force::Off };
        Ok(Remove {
            directory: workspace::host_path(Path::new(&input.directory))?,
            force,
        })
    }

    /// The list input's absolute `source`, from its stdin bytes. Else `E_INVALID_ARG`.
    pub fn read_list(stdin: &[u8]) -> Result<Utf8PathBuf, OpError> {
        let input: ListInput = parse(stdin, "list", HOST)?;
        workspace::host_path(Path::new(&input.source))
    }

    /// Makes the workspace `input` asks for at its destination, bootstrapped as `bootstrap`
    /// says. The same request again (same source and destination) is the same workspace, not
    /// bootstrapped again; a failed create leaves nothing it made.
    pub fn create(ctx: &Ctx<'_>, input: &Create, bootstrap: Bootstrap) -> Result<Made, OpError> {
        // 1. The starting ref, held by the store: no other base is taken in its place. An
        //    empty ref or name is none.
        if let Some(start) = given(input.start.as_deref()) {
            held(ctx, start)?;
        }
        let start = start_of(ctx, &input.source, given(input.start.as_deref()))?;

        // 2. The request: the same source and destination is the same workspace.
        let (source, destination) = (input.source.as_str(), input.destination.as_str());
        let session = digest(
            &[FAMILY.as_bytes(), source.as_bytes(), destination.as_bytes()],
            SESSION_LEN,
        );
        if let Some(same) = again(ctx, &input.destination, &session)? {
            return Ok(same);
        }

        // 3. Its name: the host's, else the destination's last component; suffixed when taken.
        let text = given(input.name.as_deref())
            .or(input.destination.file_name())
            .unwrap_or_default();
        let suffix = digest(
            &[source.as_bytes(), destination.as_bytes(), text.as_bytes()],
            SUFFIX_LEN,
        );
        let name = free_name(ctx, slug(text), &session, &suffix)?;

        // 4. Made as the family's, a failed bootstrap taken back.
        let making = Making {
            who: family_member(FAMILY, &name)?,
            name,
            dir: input.destination.clone(),
            start,
            from: &input.source,
            session,
        };
        let made = make(ctx, making, bootstrap, Failed::Remove)?;
        Ok(Made {
            path: input.destination.clone(),
            name: made.name,
            head: made.head,
        })
    }

    /// Removes the family's workspace `input` names (see [`Force`]); `E_INVALID_ARG` for one
    /// another family, or a person, made. A directory that is no workspace is
    /// [`super::ours`]'s none, which the CLI answers before this.
    pub fn remove(ctx: &Ctx<'_>, input: &Remove) -> Result<RemoveReport, OpError> {
        take_down(ctx, &input.directory, Owner::Family(FAMILY), input.force)
    }

    /// The directories the host may show for `source`: itself, then the family's workspaces
    /// whose directory exists, by path. Writes nothing.
    pub fn list(ctx: &Ctx<'_>, source: &Utf8Path) -> Result<Vec<Shown>, OpError> {
        // 1. The family's entries whose directory is there.
        let registry = Registry::open(&ctx.store_root, Arc::clone(&ctx.fs));
        let mut paths = Vec::new();
        for entry in registry.list()? {
            if !of_family(&entry, FAMILY) {
                continue;
            }
            let path = Utf8PathBuf::from(entry.path);
            let meta = (ctx.host.symlink_metadata(&path))
                .map_err(|e| materialize::host_failed(path.as_str(), &e))?;
            if meta.is_some_and(|meta| meta.kind == HostKind::Dir) {
                paths.push(path);
            }
        }
        paths.sort();

        // 2. The root first.
        let root = Shown {
            path: source.to_owned(),
            kind: Kind::Root,
        };
        let shown = paths.into_iter().map(|path| Shown {
            path,
            kind: Kind::Workspace,
        });
        Ok(std::iter::once(root).chain(shown).collect())
    }

    /// `text` when given and not empty.
    fn given(text: Option<&str>) -> Option<&str> {
        text.filter(|text| !text.is_empty())
    }

    /// `E_REF_NOT_FOUND` naming `start` when the store holds no such ref or changeset.
    fn held(ctx: &Ctx<'_>, start: &str) -> Result<(), OpError> {
        match ctx.store.resolve_rev(ResolveRevReq::new(start)) {
            Ok(_) => Ok(()),
            Err(e) if NOT_HELD.iter().any(|code| e.code == code.as_str()) => Err(OpError::new(
                ErrorCode::RefNotFound,
                format!("the store holds no ref {start:?} to start from"),
            )
            .with_details(Map::from_iter([("ref".to_owned(), Value::from(start))]))
            .with_hint("swarmops ref list")),
            Err(e) => Err(e),
        }
    }

    /// The codes of a revision the store does not hold.
    const NOT_HELD: [ErrorCode; 2] = [ErrorCode::RevNotFound, ErrorCode::RefNotFound];

    /// The workspace at `destination` when it answers the same request (`session`); none when
    /// nothing is there; `E_WS_EXISTS` for anything else there.
    fn again(
        ctx: &Ctx<'_>,
        destination: &Utf8Path,
        session: &str,
    ) -> Result<Option<Made>, OpError> {
        // 1. Nothing there: a new workspace.
        let there = (ctx.host.symlink_metadata(destination))
            .map_err(|e| materialize::host_failed(destination.as_str(), &e))?;
        if there.is_none() {
            return Ok(None);
        }

        // 2. The family's workspace of this request, and of this store.
        let file = match state::read(ctx.fs.as_ref(), destination) {
            Ok(file) => file,
            Err(e) if e.code == ErrorCode::WsNotFound.as_str() => return Err(taken(destination)),
            Err(e) => return Err(e),
        };
        let registry = Registry::open(&ctx.store_root, Arc::clone(&ctx.fs));
        let entry = registry.read(&file.name)?;
        let ours = entry.is_some_and(|entry| {
            entry.path == destination.as_str()
                && entry.session.as_deref() == Some(session)
                && of_family(&entry, FAMILY)
        });
        let repo_id = ctx.store.repo_info(InfoReq::default())?.repo_id;
        if !ours || file.repo_id != repo_id {
            return Err(taken(destination));
        }
        Ok(Some(Made {
            path: destination.to_owned(),
            name: file.name,
            head: file.head,
        }))
    }

    /// `E_WS_EXISTS` for a destination that holds something already.
    fn taken(destination: &Utf8Path) -> OpError {
        OpError::new(
            ErrorCode::WsExists,
            format!("{destination} exists already, and is no workspace this request made"),
        )
        .with_details(Map::from_iter([(
            "path".to_owned(),
            Value::from(destination.as_str()),
        )]))
        .with_hint("ask for another destination")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Any text becomes a workspace name of at most 56 bytes, which a suffix keeps within 64;
    /// one that would be none gets the `ws-` prefix.
    #[test]
    fn slug_makes_a_workspace_name_of_any_text() {
        // 1. The grammar's characters kept, the others made `-`, capitals lowered.
        assert_eq!(slug("Feature Auth"), "feature-auth");
        assert_eq!(slug("bold-oak-a3f2"), "bold-oak-a3f2");
        assert_eq!(slug("ünï/cödé"), "-n--c-d-");

        // 2. No name otherwise: empty, a leading dot, a device name, a trailing dot.
        assert_eq!(slug(""), "ws-");
        assert_eq!(slug(".env"), "ws-.env");
        assert_eq!(slug("CON"), "ws-con");
        assert_eq!(slug("v1."), "v1-");

        // 3. Cut, with room for the suffix.
        let long = slug(&"x".repeat(100));
        assert_eq!(long.len(), MAX_SLUG);
        const { assert!(MAX_SLUG + 1 + SUFFIX_LEN <= 64) };
        assert_eq!(slug(&format!(".{}", "y".repeat(100))).len(), MAX_SLUG);
    }

    /// The store's directory is its name made a slug, or its id's first 12 digits.
    #[test]
    fn repo_dir_is_the_name_or_the_id() {
        assert_eq!(repo_dir("Acme Web", "0123456789abcdef"), "acme-web");
        assert_eq!(repo_dir("", "0123456789abcdef"), "0123456789ab");
    }
}
