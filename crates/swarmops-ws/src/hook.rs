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

use std::path::Path;
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use swarmops_api::{ErrorCode, Identity, InfoReq, OpError, StatusAfter, WsCreateReq, WsDeleteReq};
use swarmops_core::portable;
use swarmops_core::{ChangesetId, ObjectId};
use swarmops_store::FsOps;

use crate::bootstrap;
use crate::registry::Registry;
use crate::workspace::{self, Found, Locate, Workspace};
use crate::{Ctx, create, delete};

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
    let input: CreateInput = parse(stdin, "WorktreeCreate")?;
    Ok(HookCreate {
        name: input.name,
        session_id: input.session_id,
        cwd: workspace::host_path(Path::new(&input.cwd))?,
    })
}

/// The `WorktreeRemove` hook's input, from its stdin bytes: at most [`MAX_HOOK_INPUT`], with an
/// absolute `worktree_path`; else `E_INVALID_ARG`.
pub fn read_remove(stdin: &[u8]) -> Result<HookRemove, OpError> {
    let input: RemoveInput = parse(stdin, "WorktreeRemove")?;
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
    let (base, prefixes, all) = if let Some(parent) = parent_of(ctx, &input.cwd)? {
        (parent.file.head, parent.file.prefixes, parent.file.all)
    } else {
        let config = bootstrap::repo_config(ctx.store, DEFAULT_BASE)?.unwrap_or_default();
        let all = config.prefixes.is_empty();
        (DEFAULT_BASE.to_owned(), config.prefixes, all)
    };

    // 2. Its name: the slug, suffixed when another session, or a hand-made workspace, holds it.
    let session = digest(&[input.session_id.as_bytes()], SESSION_LEN);
    let registry = Registry::open(&ctx.store_root, Arc::clone(&ctx.fs));
    let mut name = slug(&input.name);
    let taken = registry.read(&name)?;
    if taken.is_some_and(|entry| entry.session.as_deref() != Some(session.as_str())) {
        let suffix = digest(
            &[input.session_id.as_bytes(), input.name.as_bytes()],
            SUFFIX_LEN,
        );
        name = format!("{name}-{suffix}");
    }

    // 3. Where, and as whom: `<ws_root>/<repo>/<name>`, the caller or `claude-code/<name>`.
    let info = ctx.store.repo_info(InfoReq::default())?;
    let dir = ws_root
        .join(repo_dir(&info.name, &info.repo_id))
        .join(&name);
    let who = match &ctx.who {
        Some(who) => who.clone(),
        None => Identity::parse(&format!("{HOOK_FAMILY}/{name}"))?,
    };
    let ctx = Ctx {
        store: ctx.store,
        store_root: ctx.store_root.clone(),
        who: Some(who),
        fs: Arc::clone(&ctx.fs),
        host: Arc::clone(&ctx.host),
    };

    // 4. Made, then bootstrapped when this create made it.
    let mut req = WsCreateReq::new(name);
    req.path = Some(dir.to_string());
    req.base = base.as_str().into();
    req.prefixes = prefixes;
    req.all = all;
    req.from = Some(input.cwd.to_string());
    req.session = Some(session);
    req.status_after = StatusAfter::None;
    let made = create::create(&ctx, req)?;
    if made.created && bootstrap == Bootstrap::Run {
        let ws = Workspace::open(&ctx, Locate::Path(dir.clone()))?;
        bootstrap::run(&ctx, &ws, Some(&input.cwd))?;
    }
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
    // 1. The workspace there, and its entry pointing at it: the delete goes by name.
    let ws = Workspace::open(ctx, Locate::Path(input.worktree_path.clone()))?;
    let name = ws.file.name;
    let registry = Registry::open(&ctx.store_root, Arc::clone(&ctx.fs));
    let entry = registry.read(&name)?;
    if entry.is_none_or(|entry| entry.path != ws.dir.as_str()) {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            format!(
                "the store's workspace {name:?} is not the one at {}",
                ws.dir
            ),
        )
        .with_details(Map::from_iter([("name".to_owned(), Value::from(name))]))
        .with_hint("swarmops ws list --check"));
    }

    // 2. Rescued and deleted.
    let mut req = WsDeleteReq::new(name);
    req.rescue = true;
    let done = delete::delete(ctx, req)?;
    let rescued = done.rescued.as_deref().map(changeset_id).transpose()?;
    Ok(RemoveReport {
        head: changeset_id(&done.head)?,
        rescued,
    })
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

/// The hook input `stdin` of `event`, parsed: `E_INVALID_ARG` past [`MAX_HOOK_INPUT`] bytes,
/// for input that is not JSON, or one without a field the hook reads.
fn parse<T: DeserializeOwned>(stdin: &[u8], event: &str) -> Result<T, OpError> {
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
            format!("the {event} hook's input is not what Claude Code sends: {e}"),
        )
    })
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
