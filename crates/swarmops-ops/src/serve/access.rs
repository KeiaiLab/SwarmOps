//! Who may do what through `serve` (M14 design §5): the store's `access.toml`, read on every
//! connection, and the caller named by [`resolve_identity`], the one resolver of `serve`'s
//! identity.

use serde::Deserialize;
use swarmops_api::{ErrorCode, Identity, OpError};
use swarmops_repo::RepoError;
use swarmops_store::{IoFailure, StoreError};
use toml::Spanned;

use super::{IdentitySource, refused};
use crate::{LocalOps, error};

/// The most bytes `access.toml` may hold: a longer one is invalid, `E_POLICY_DENIED`.
pub const MAX_ACCESS_BYTES: u64 = 1 << 20;

/// The file's name in the store's root.
const ACCESS_TOML: &str = "access.toml";

/// The only `version` of `access.toml` this build reads.
const VERSION: i64 = 1;

/// The ref a `force` pattern may never cover: a non-fast-forward on `main` is never allowed.
const MAIN: &str = "main";

/// A pattern's segment that matches any one segment.
const ONE: &str = "*";

/// A pattern's last segment that matches every depth below.
const DEEP: &str = "**";

/// What the hint of a connection that names no caller says to do.
const NO_IDENTITY_HINT: &str = "name the caller with --as <family>/<instance> on serve's command \
line: the command= of its authorized_keys line, or the web server's CGI entry";

/// What the hint of a refusal for a missing or invalid `access.toml` says to do.
const ACCESS_HINT: &str = "the operator writes access.toml in the store's root: version = 1, then \
one [[grant]] per set of callers (who, read, push, force, land)";

/// What a caller asks to do with a ref.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Action {
    /// Fetch or clone it.
    Read,
    /// Move it forward.
    Push,
    /// Move it otherwise than forward.
    Force,
    /// Delete it.
    Delete,
    /// Land onto it, through `refs/land/<ref>`.
    Land,
}

impl Action {
    /// The name a refusal's `details.action` gives it.
    fn as_str(self) -> &'static str {
        match self {
            Action::Read => "read",
            Action::Push => "push",
            Action::Force => "force",
            Action::Delete => "delete",
            Action::Land => "land",
        }
    }
}

/// A store's grants, as `access.toml` gives them.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Access {
    /// Every `[[grant]]`, in file order (order never matters: grants form a union).
    grants: Vec<Grant>,
}

/// One `[[grant]]`: what the callers its `who` names may do.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Grant {
    /// The callers: identities, or `<family>/*`.
    who: Vec<Pattern>,
    /// Whether they may fetch, also implied by any of the three lists below.
    read: bool,
    /// The refs a push may create and fast-forward.
    push: Vec<Pattern>,
    /// Of the refs `push` covers, those a push may also move backwards or delete.
    force: Vec<Pattern>,
    /// The targets a push to `refs/land/<target>` may land onto.
    land: Vec<Pattern>,
}

impl Grant {
    /// Whether the grant gives any reading at all: `read`, or a list that implies it.
    fn reads(&self) -> bool {
        self.read || !self.push.is_empty() || !self.force.is_empty() || !self.land.is_empty()
    }
}

/// A name pattern: `/`-separated segments, each exact or `*`, the last one possibly `**`.
///
/// `feature/*` matches `feature/a`, not `feature/a/b`; `feature/**` matches both, not `feature`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pattern {
    /// The pattern's segments.
    segments: Vec<String>,
}

impl Pattern {
    /// The pattern `text`, or `None` if a segment is empty, holds a `*` beside other bytes, or
    /// is a `**` that is not the last one.
    fn parse(text: &str) -> Option<Pattern> {
        let segments: Vec<String> = text.split('/').map(str::to_owned).collect();
        let last = segments.len() - 1;
        for (i, segment) in segments.iter().enumerate() {
            let wild = segment == ONE || (segment == DEEP && i == last);
            if segment.is_empty() || (segment.contains('*') && !wild) {
                return None;
            }
        }
        Some(Pattern { segments })
    }

    /// Whether `name` matches.
    fn matches(&self, name: &str) -> bool {
        let names: Vec<&str> = name.split('/').collect();

        // 1. A trailing `**`: the segments before it, then at least one below.
        if let Some((last, head)) = self.segments.split_last()
            && last == DEEP
        {
            return names.len() > head.len() && segments_match(head, &names[..head.len()]);
        }

        // 2. Otherwise segment for segment.
        names.len() == self.segments.len() && segments_match(&self.segments, &names)
    }
}

/// Whether each name segment matches its pattern segment, exactly or by `*`.
fn segments_match(patterns: &[String], names: &[&str]) -> bool {
    patterns
        .iter()
        .zip(names)
        .all(|(pattern, name)| pattern == ONE || pattern == name)
}

/// `access.toml` as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    version: Spanned<i64>,
    #[serde(default)]
    grant: Vec<GrantFile>,
}

/// One `[[grant]]` as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantFile {
    who: Vec<Spanned<String>>,
    #[serde(default)]
    read: bool,
    #[serde(default)]
    push: Vec<Spanned<String>>,
    #[serde(default)]
    force: Vec<Spanned<String>>,
    #[serde(default)]
    land: Vec<Spanned<String>>,
}

impl Access {
    /// The grants of the store `ops` serves, read now through the store's filesystem seam.
    /// No file is `E_AUTH` `no_access_file`; a file this build does not read is
    /// `E_POLICY_DENIED` `access_file_invalid`, with its line and no path.
    pub fn load(ops: &LocalOps) -> Result<Access, OpError> {
        // 1. The bytes, bounded before they are read; no file grants nothing.
        let fs = ops.repo.store_fs();
        let path = fs.root().join(ACCESS_TOML);
        if !fs.exists(&path).map_err(io)? {
            return Err(no_access_file());
        }
        let len = fs.file_len(&path).map_err(io)?;
        if len > MAX_ACCESS_BYTES {
            return Err(too_large(len));
        }
        let Some(bytes) = fs.read(&path).map_err(io)? else {
            return Err(no_access_file());
        };
        let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if len > MAX_ACCESS_BYTES {
            return Err(too_large(len));
        }

        // 2. Text, then the shape and the version.
        let text = String::from_utf8(bytes).map_err(|e| {
            let at = e.utf8_error().valid_up_to();
            invalid(line_of(e.as_bytes(), at), "is not UTF-8")
        })?;
        let file: File = toml::from_str(&text).map_err(|e| {
            let at = e.span().map_or(0, |span| span.start);
            invalid(line_of(text.as_bytes(), at), e.message())
        })?;
        if *file.version.get_ref() != VERSION {
            let line = line_of(text.as_bytes(), file.version.span().start);
            let found = file.version.get_ref();
            return Err(invalid(
                line,
                &format!("has version {found}, not {VERSION}"),
            ));
        }

        // 3. Each grant's patterns, and no force on main.
        let grants = (file.grant.into_iter())
            .map(|grant| grant_of(&text, grant))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Access { grants })
    }

    /// Whether `who` may do `action` with the store ref `name`. `E_AUTH` if no grant names
    /// `who`; `E_POLICY_DENIED` `{identity, action, ref}` if the grants that do give no such
    /// right.
    pub fn check(&self, who: &Identity, action: Action, name: &str) -> Result<(), OpError> {
        // 1. The grants that name the caller.
        let agent = who.agent();
        let mine: Vec<&Grant> = (self.grants.iter())
            .filter(|grant| grant.who.iter().any(|p| p.matches(agent)))
            .collect();
        if mine.is_empty() {
            return Err(refused(
                ErrorCode::Auth,
                "unknown_identity",
                format!("no grant of access.toml names {agent}"),
            )
            .with_hint(ACCESS_HINT));
        }

        // 2. Their union: force only on refs a push covers, delete as force.
        let covers = |list: fn(&Grant) -> &[Pattern]| {
            (mine.iter()).any(|grant| list(grant).iter().any(|p| p.matches(name)))
        };
        let granted = match action {
            Action::Read => mine.iter().any(|grant| grant.reads()),
            Action::Push => covers(|g| &g.push),
            Action::Force | Action::Delete => covers(|g| &g.push) && covers(|g| &g.force),
            Action::Land => covers(|g| &g.land),
        };
        if granted {
            return Ok(());
        }
        let mut error = refused(
            ErrorCode::PolicyDenied,
            "not_granted",
            format!("{agent} may not {} {name}", action.as_str()),
        );
        error.details.insert("identity".to_owned(), agent.into());
        error
            .details
            .insert("action".to_owned(), action.as_str().into());
        error.details.insert("ref".to_owned(), name.into());
        Err(error)
    }
}

/// The grant `file` gives, its patterns checked against `text`'s lines.
fn grant_of(text: &str, file: GrantFile) -> Result<Grant, OpError> {
    let patterns = |list: Vec<Spanned<String>>| {
        (list.into_iter())
            .map(|item| {
                let line = line_of(text.as_bytes(), item.span().start);
                Pattern::parse(item.get_ref()).ok_or_else(|| {
                    invalid(line, &format!("has a bad pattern {:?}", item.get_ref()))
                })
            })
            .collect::<Result<Vec<_>, _>>()
    };

    // 1. A force that covers main is never valid.
    for item in &file.force {
        let covers_main = Pattern::parse(item.get_ref()).is_some_and(|p| p.matches(MAIN));
        if covers_main {
            let line = line_of(text.as_bytes(), item.span().start);
            return Err(invalid(
                line,
                &format!("has force {:?}, which covers main", item.get_ref()),
            ));
        }
    }

    // 2. Every pattern parsed.
    Ok(Grant {
        who: patterns(file.who)?,
        read: file.read,
        push: patterns(file.push)?,
        force: patterns(file.force)?,
        land: patterns(file.land)?,
    })
}

/// The 1-based line of byte `at` in `bytes`.
fn line_of(bytes: &[u8], at: usize) -> u64 {
    let before = bytes.get(..at).unwrap_or(bytes);
    let lines = before.split(|&b| b == b'\n').count();
    u64::try_from(lines).unwrap_or(u64::MAX)
}

/// `E_AUTH` `no_access_file`: the store has no `access.toml`, so no caller may do anything.
fn no_access_file() -> OpError {
    refused(
        ErrorCode::Auth,
        "no_access_file",
        "the store has no access.toml: serve grants nothing without one",
    )
    .with_hint(ACCESS_HINT)
}

/// `E_POLICY_DENIED` `access_file_invalid`: `access.toml` `what`, at `line`.
fn invalid(line: u64, what: &str) -> OpError {
    let mut error = refused(
        ErrorCode::PolicyDenied,
        "access_file_invalid",
        error::one_line(format!("access.toml {what} (line {line})")),
    )
    .with_hint(ACCESS_HINT);
    error.details.insert("line".to_owned(), line.into());
    error
}

/// `E_POLICY_DENIED` `access_file_invalid` for a file of `len` bytes, over the limit. It is not
/// read, so it names the limit instead of a line.
fn too_large(len: u64) -> OpError {
    let mut error = refused(
        ErrorCode::PolicyDenied,
        "access_file_invalid",
        format!("access.toml is {len} bytes, more than {MAX_ACCESS_BYTES}"),
    )
    .with_hint(ACCESS_HINT);
    error.details.insert("bytes".to_owned(), len.into());
    error
        .details
        .insert("limit".to_owned(), MAX_ACCESS_BYTES.into());
    error
}

/// The operations-spec error for a filesystem failure on `access.toml`: its path relative to
/// the store's root.
fn io(e: IoFailure) -> OpError {
    error::op(RepoError::from(StoreError::from(e)))
}

/// The caller `source` names: `--as`'s identity, or `<family>/<remote_user>` under
/// `--user-family`. Naming none is `E_AUTH` `no_identity`.
pub fn resolve_identity(
    source: &IdentitySource,
    remote_user: Option<&str>,
) -> Result<Identity, OpError> {
    match source {
        IdentitySource::Fixed(who) => Ok(who.clone()),
        IdentitySource::UserFamily(family) => user_family(family, remote_user),
        IdentitySource::Absent => Err(refused(
            ErrorCode::Auth,
            "no_identity",
            "serve names no caller: --as is not given, and SWARMOPS_AGENT does not name one for \
             serve",
        )
        .with_hint(NO_IDENTITY_HINT)),
    }
}

/// `<family>/<remote_user>`: `E_AUTH` if the web server names no user, or one the instance
/// grammar refuses.
fn user_family(family: &str, remote_user: Option<&str>) -> Result<Identity, OpError> {
    let Some(user) = remote_user else {
        return Err(refused(
            ErrorCode::Auth,
            "no_remote_user",
            "serve --user-family names no caller: the web server sets no REMOTE_USER",
        )
        .with_hint("authenticate the request in the web server, which then sets REMOTE_USER"));
    };
    Identity::parse(&format!("{family}/{user}")).map_err(|_| {
        refused(
            ErrorCode::Auth,
            "remote_user_invalid",
            error::one_line(format!(
                "REMOTE_USER {user:?} is not an instance of family {family}"
            )),
        )
        .with_hint("instances are [A-Za-z0-9._-]+, not starting with '.'")
    })
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;
    use swarmops_api::{ErrorCode, Identity, OpError};

    use super::{Access, Action, MAX_ACCESS_BYTES, resolve_identity};
    use crate::LocalOps;
    use crate::serve::IdentitySource;

    /// The grant of the design's example: every rule of access.toml at once.
    const EXAMPLE: &str = r#"version = 1

[[grant]]
who = ["person/ada", "ci/*"]
read = true
push = ["release/*", "feature/**", "tags/**"]
force = ["feature/**"]
land = ["main", "release/*"]
"#;

    /// A fresh store, its root, and the directory that holds it.
    fn store() -> (tempfile::TempDir, Utf8PathBuf, LocalOps) {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
        let (ops, _) = LocalOps::init(&root, "test").unwrap();
        (dir, root, ops)
    }

    /// The grants of `text` as the store's access.toml.
    fn load(text: &str) -> Result<Access, OpError> {
        let (_dir, root, ops) = store();
        std::fs::write(root.join("access.toml"), text).unwrap();
        Access::load(&ops)
    }

    fn who(agent: &str) -> Identity {
        Identity::parse(agent).unwrap()
    }

    /// Whether `agent` may do `action` on `name` under `access`.
    fn may(access: &Access, agent: &str, action: Action, name: &str) -> bool {
        access.check(&who(agent), action, name).is_ok()
    }

    /// `error` is `access_file_invalid` at `line`, and names no host path.
    fn assert_invalid(error: &OpError, line: u64) {
        assert_eq!(error.code, ErrorCode::PolicyDenied.as_str(), "{error:?}");
        assert_eq!(error.details["reason"], "access_file_invalid", "{error:?}");
        assert_eq!(error.details["line"], line, "{error:?}");
        assert_no_path(error);
    }

    /// Nothing `error` says holds a host path: an absolute one, or the temp directory's.
    fn assert_no_path(error: &OpError) {
        let text = serde_json::to_string(error).unwrap();
        let tmp = std::env::temp_dir();
        assert!(!text.contains(tmp.to_str().unwrap()), "{text}");
        assert!(!text.contains("/store"), "{text}");
    }

    /// An exact pattern matches its name alone.
    #[test]
    fn exact_name() {
        let access = load(EXAMPLE).unwrap();
        assert!(may(&access, "person/ada", Action::Land, "main"));
        assert!(!may(&access, "person/ada", Action::Land, "mainline"));
        assert!(!may(&access, "person/ada", Action::Land, "main/x"));
        assert!(!may(&access, "person/bob", Action::Read, "main"));
    }

    /// `*` matches one segment, a family's instances among them.
    #[test]
    fn star_is_one_segment() {
        let access = load(EXAMPLE).unwrap();
        assert!(may(&access, "person/ada", Action::Push, "release/2"));
        assert!(!may(&access, "person/ada", Action::Push, "release/2/x"));
        assert!(!may(&access, "person/ada", Action::Push, "release"));
        assert!(may(&access, "ci/runner-1", Action::Push, "release/2"));
    }

    /// A trailing `**` matches every depth below, not the name above it.
    #[test]
    fn trailing_double_star_is_every_depth() {
        let access = load(EXAMPLE).unwrap();
        assert!(may(&access, "person/ada", Action::Push, "feature/a"));
        assert!(may(&access, "person/ada", Action::Push, "feature/a/b/c"));
        assert!(!may(&access, "person/ada", Action::Push, "feature"));
        assert!(!may(&access, "person/ada", Action::Push, "features/a"));
    }

    /// A `**` before the end, and a `*` beside other bytes, make the file invalid.
    #[test]
    fn bad_patterns_are_invalid() {
        for pattern in ["a/**/b", "fe*", "a//b", ""] {
            let text =
                format!("version = 1\n[[grant]]\nwho = [\"person/ada\"]\npush = [{pattern:?}]\n");
            assert_invalid(&load(&text).unwrap_err(), 4);
        }
    }

    /// Grants form a union: what one grant gives, another does not take away.
    #[test]
    fn grants_form_a_union() {
        let text = r#"version = 1
[[grant]]
who = ["person/ada"]
push = ["a"]
[[grant]]
who = ["person/*"]
push = ["b"]
"#;
        let access = load(text).unwrap();
        assert!(may(&access, "person/ada", Action::Push, "a"));
        assert!(may(&access, "person/ada", Action::Push, "b"));
        assert!(!may(&access, "person/bob", Action::Push, "a"));
        assert!(may(&access, "person/bob", Action::Push, "b"));
    }

    /// `push`, `force` and `land` each imply `read`; a grant of none of them reads nothing.
    #[test]
    fn lists_imply_read() {
        for list in ["push", "force", "land"] {
            let text =
                format!("version = 1\n[[grant]]\nwho = [\"person/ada\"]\n{list} = [\"x\"]\n");
            let access = load(&text).unwrap();
            assert!(may(&access, "person/ada", Action::Read, "main"), "{list}");
        }
        let bare = load("version = 1\n[[grant]]\nwho = [\"person/ada\"]\n").unwrap();
        assert!(!may(&bare, "person/ada", Action::Read, "main"));
    }

    /// `force` acts only on refs `push` covers, and grants deletion there: alone it grants
    /// nothing but reading.
    #[test]
    fn force_needs_push() {
        let access = load(EXAMPLE).unwrap();
        for action in [Action::Force, Action::Delete] {
            assert!(
                may(&access, "person/ada", action, "feature/a"),
                "{action:?}"
            );
            assert!(
                !may(&access, "person/ada", action, "release/2"),
                "{action:?}"
            );
        }

        let alone = "version = 1\n[[grant]]\nwho = [\"person/ada\"]\nforce = [\"x\"]\n";
        let access = load(alone).unwrap();
        for action in [Action::Push, Action::Force, Action::Delete, Action::Land] {
            assert!(!may(&access, "person/ada", action, "x"), "{action:?}");
        }
    }

    /// No access.toml refuses every caller: `E_AUTH` `no_access_file`, with no path.
    #[test]
    fn no_file_is_e_auth() {
        let (_dir, _root, ops) = store();
        let error = Access::load(&ops).unwrap_err();
        assert_eq!(error.code, ErrorCode::Auth.as_str(), "{error:?}");
        assert_eq!(error.details["reason"], "no_access_file", "{error:?}");
        assert_no_path(&error);
    }

    /// An unknown key is invalid, at its line.
    #[test]
    fn unknown_key_is_invalid() {
        let top = "version = 1\nowner = \"x\"\n";
        assert_invalid(&load(top).unwrap_err(), 2);
        let grant = "version = 1\n[[grant]]\nwho = [\"person/ada\"]\ndelete = [\"x\"]\n";
        assert_invalid(&load(grant).unwrap_err(), 4);
    }

    /// A version other than 1 is invalid, at its line.
    #[test]
    fn version_2_is_invalid() {
        assert_invalid(&load("# grants\nversion = 2\n").unwrap_err(), 2);
    }

    /// A `force` that covers `main` is invalid, at its line, however it covers it.
    #[test]
    fn force_on_main_is_invalid() {
        for pattern in ["main", "*", "**"] {
            let text = format!(
                "version = 1\n[[grant]]\nwho = [\"person/ada\"]\npush = [\"**\"]\nforce = [{pattern:?}]\n"
            );
            assert_invalid(&load(&text).unwrap_err(), 5);
        }
    }

    /// One byte over `MAX_ACCESS_BYTES` is invalid, with no path; the limit itself is read.
    #[test]
    fn over_the_limit_is_invalid() {
        let limit = usize::try_from(MAX_ACCESS_BYTES).unwrap();
        let mut text = EXAMPLE.to_owned();
        text.push_str(&"#".repeat(limit - text.len()));
        assert!(load(&text).is_ok());

        text.push('#');
        let error = load(&text).unwrap_err();
        assert_eq!(error.code, ErrorCode::PolicyDenied.as_str(), "{error:?}");
        assert_eq!(error.details["reason"], "access_file_invalid", "{error:?}");
        assert_eq!(error.details["limit"], MAX_ACCESS_BYTES, "{error:?}");
        assert_no_path(&error);
    }

    /// A caller no grant names is `E_AUTH`, whatever it asks.
    #[test]
    fn unnamed_caller_is_e_auth() {
        let access = load(EXAMPLE).unwrap();
        let error = (access.check(&who("person/bob"), Action::Read, "main")).unwrap_err();
        assert_eq!(error.code, ErrorCode::Auth.as_str(), "{error:?}");
    }

    /// A named caller without the action is `E_POLICY_DENIED`, `{identity, action, ref}`.
    #[test]
    fn named_caller_without_the_action_is_denied() {
        let access = load(EXAMPLE).unwrap();
        let error = (access.check(&who("ci/runner-1"), Action::Delete, "release/2")).unwrap_err();
        assert_eq!(error.code, ErrorCode::PolicyDenied.as_str(), "{error:?}");
        assert_eq!(error.details["identity"], "ci/runner-1", "{error:?}");
        assert_eq!(error.details["action"], "delete", "{error:?}");
        assert_eq!(error.details["ref"], "release/2", "{error:?}");
    }

    /// `--user-family person` with the web server's user `ada` names `person/ada`.
    #[test]
    fn user_family_names_family_slash_user() {
        let source = IdentitySource::UserFamily("person".to_owned());
        assert_eq!(
            resolve_identity(&source, Some("ada")).unwrap(),
            who("person/ada")
        );
    }

    /// No REMOTE_USER, or one outside the instance grammar, names no caller: `E_AUTH`. The
    /// resolver takes no environment, so SWARMOPS_AGENT cannot stand in for it.
    #[test]
    fn user_family_without_a_usable_user_is_e_auth() {
        let source = IdentitySource::UserFamily("person".to_owned());
        for remote_user in [None, Some(""), Some(".hidden"), Some("a/b"), Some("a b")] {
            let error = resolve_identity(&source, remote_user).unwrap_err();
            assert_eq!(
                error.code,
                ErrorCode::Auth.as_str(),
                "{remote_user:?}: {error:?}"
            );
        }
    }

    /// No `--as` names no caller, whatever a web server's user is: `E_AUTH` `no_identity`.
    #[test]
    fn absent_is_e_auth_no_identity() {
        for remote_user in [None, Some("ada")] {
            let error = resolve_identity(&IdentitySource::Absent, remote_user).unwrap_err();
            assert_eq!(error.code, ErrorCode::Auth.as_str(), "{error:?}");
            assert_eq!(error.details["reason"], "no_identity", "{error:?}");
        }
    }

    /// `--as` names the caller, whatever a web server's user is.
    #[test]
    fn fixed_is_its_identity() {
        let who = Identity::parse("person/ada").unwrap();
        let source = IdentitySource::Fixed(who.clone());
        assert_eq!(resolve_identity(&source, None).unwrap(), who);
        assert_eq!(resolve_identity(&source, Some("bob")).unwrap(), who);
    }
}
