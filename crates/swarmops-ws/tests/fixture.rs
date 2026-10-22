//! The fixture the workspace tests share: a store made through `LocalOps::init` in a temporary
//! directory and a workspace made through `create`, over the real file layers or a double (first
//! written by the M5 plan's task W5, then W6's edit helpers and W10's sweep driver).

use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_api::{
    ChangeInput, ChangeOp, CommitReq, ContentInput, Identity, ModeInput, OpError, StatusAfter,
    StoreOps, WsCreateReq, WsInfo,
};
use swarmops_ops::LocalOps;
use swarmops_store::{FsOps, RealFs};
use swarmops_ws::state::{self, Volume, WorkspaceFile};
use swarmops_ws::workspace::{Locate, Workspace};
use swarmops_ws::{Ctx, HostFs, RealHost};

/// The identity the fixture's mutations act as.
pub const AGENT: &str = "agent/tester";

/// One entry of a commit the fixture makes: a path and what it holds, or its removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Put<'a> {
    /// A regular file holding the text.
    File(&'a str, &'a str),
    /// An executable file holding the text.
    Exec(&'a str, &'a str),
    /// A symlink holding the target.
    Link(&'a str, &'a str),
    /// The path removed.
    Gone(&'a str),
}

/// A temporary directory and a store made in it, at `<tmp>/store`; workspaces go beside it.
pub struct Fixture {
    _tmp: tempfile::TempDir,
    /// The store's root.
    pub root: Utf8PathBuf,
    /// The store's operations.
    pub ops: LocalOps,
}

impl Fixture {
    /// A fresh store named `acme`, without a changeset.
    pub fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap().join("store");
        let (ops, _) = LocalOps::init(&root, "acme").unwrap();
        Fixture {
            _tmp: tmp,
            root,
            ops,
        }
    }

    /// What the workspace operations run with, over the host `host`, as the fixture's agent.
    pub fn ctx_on(&self, host: Arc<dyn HostFs>) -> Ctx<'_> {
        self.ctx_with(host, Arc::new(RealFs))
    }

    /// What the workspace operations run with, over the host `host` and the durable files' `fs`.
    pub fn ctx_with(&self, host: Arc<dyn HostFs>, fs: Arc<dyn FsOps>) -> Ctx<'_> {
        Ctx {
            store: &self.ops,
            store_root: self.root.clone(),
            who: Some(Identity::parse(AGENT).unwrap()),
            fs,
            host,
        }
    }

    /// What the workspace operations run with, over the real host.
    pub fn ctx(&self) -> Ctx<'_> {
        self.ctx_on(Arc::new(RealHost))
    }

    /// A directory beside the store, not made.
    pub fn beside(&self, name: &str) -> Utf8PathBuf {
        self.root.parent().unwrap().join(name)
    }

    /// Commits `puts` on `parent` (a root commit when none) and moves `main` to it.
    pub fn commit(&self, parent: Option<&str>, puts: &[Put<'_>]) -> String {
        // 1. The change list: a put of inline content in its mode, or a delete.
        let changes = (puts.iter())
            .map(|put| {
                let (path, text, mode) = match *put {
                    Put::File(path, text) => (path, text, ModeInput::File),
                    Put::Exec(path, text) => (path, text, ModeInput::Exec),
                    Put::Link(path, target) => (path, target, ModeInput::Symlink),
                    Put::Gone(path) => {
                        return ChangeInput {
                            path: path.to_owned(),
                            op: ChangeOp::Delete,
                            content: None,
                            mode: None,
                        };
                    }
                };
                ChangeInput {
                    path: path.to_owned(),
                    op: ChangeOp::Put,
                    content: Some(ContentInput::Inline(text.to_owned())),
                    mode: Some(mode),
                }
            })
            .collect();

        // 2. Recorded on the parent, `main` moved along.
        let req = CommitReq {
            parent: parent.map(Into::into),
            changes,
            message: "fixture".to_owned(),
            advance: Some("main".to_owned()),
            idempotency_key: None,
            allow_empty: false,
        };
        let who = Identity::parse(AGENT).unwrap();
        self.ops.commit(&who, req).unwrap().changeset
    }

    /// Makes workspace `name` beside the store, on `base`, holding `prefixes` (the whole tree
    /// when empty), through `ctx`.
    pub fn create_in(
        &self,
        ctx: &Ctx<'_>,
        name: &str,
        base: &str,
        prefixes: &[&str],
    ) -> Result<WsInfo, OpError> {
        let req = WsCreateReq {
            name: name.to_owned(),
            path: Some(self.beside(name).to_string()),
            base: base.into(),
            prefixes: prefixes.iter().map(|p| (*p).to_owned()).collect(),
            all: prefixes.is_empty(),
            from: None,
            session: None,
            status_after: StatusAfter::None,
        };
        swarmops_ws::create(ctx, req)
    }

    /// Makes workspace `name` beside the store over the real host, as [`Fixture::create_in`].
    pub fn create(&self, name: &str, base: &str, prefixes: &[&str]) -> WsInfo {
        self.create_in(&self.ctx(), name, base, prefixes).unwrap()
    }

    /// Workspace `name`, opened.
    pub fn open(ctx: &Ctx<'_>, name: &str) -> Workspace {
        Workspace::open(ctx, Locate::Name(name.to_owned())).unwrap()
    }

    /// Records `volume` in workspace `name`'s state, as a volume that holds it would have been
    /// probed: how the tests simulate a volume this host has none of.
    pub fn simulate(&self, name: &str, volume: Volume) {
        let dir = self.beside(name);
        let file = WorkspaceFile {
            volume,
            ..state::read(&RealFs, &dir).unwrap()
        };
        state::replace(&RealFs, &dir, &file).unwrap();
    }
}

/// The files below `dir`, by repository path, with what each is (`f` a file, `x` an executable,
/// `l` a symlink) and holds: `.swarmws/` left out.
pub fn tree_of(dir: &Utf8Path) -> Vec<(String, char, String)> {
    let mut found = Vec::new();
    walk(dir, dir, &mut found);
    found.sort();
    found
}

/// [`tree_of`]'s walk below `at`.
fn walk(root: &Utf8Path, at: &Utf8Path, found: &mut Vec<(String, char, String)>) {
    for entry in at.read_dir_utf8().unwrap() {
        // 1. The path below the root; the workspace's own directory left out.
        let entry = entry.unwrap();
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap().as_str().to_owned();
        if rel == swarmops_ws::state::SWARMWS_DIR {
            continue;
        }

        // 2. What is there, never following a symlink.
        let meta = std::fs::symlink_metadata(path).unwrap();
        if meta.is_dir() {
            walk(root, path, found);
        } else if meta.file_type().is_symlink() {
            let target = std::fs::read_link(path).unwrap();
            found.push((rel, 'l', target.to_str().unwrap().to_owned()));
        } else {
            let text = String::from_utf8(std::fs::read(path).unwrap()).unwrap();
            found.push((rel, kind_of(&meta), text));
        }
    }
}

/// `x` for an executable file, `f` for any other.
#[cfg(unix)]
fn kind_of(meta: &std::fs::Metadata) -> char {
    use std::os::unix::fs::PermissionsExt;
    if meta.permissions().mode() & 0o111 == 0 {
        'f'
    } else {
        'x'
    }
}

/// `f`: no executable bit where the platform has none.
#[cfg(not(unix))]
fn kind_of(_meta: &std::fs::Metadata) -> char {
    'f'
}

/// The expected [`tree_of`] row of a file.
pub fn file(path: &str, text: &str) -> (String, char, String) {
    (path.to_owned(), 'f', text.to_owned())
}

/// The expected [`tree_of`] row of an executable.
pub fn exec(path: &str, text: &str) -> (String, char, String) {
    (path.to_owned(), 'x', text.to_owned())
}

/// The expected [`tree_of`] row of a symlink.
pub fn link(path: &str, target: &str) -> (String, char, String) {
    (path.to_owned(), 'l', target.to_owned())
}
