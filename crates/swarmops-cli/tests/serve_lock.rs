//! What a served push does while it holds `git/serve.lock`, counted (the slow suite's
//! `served_traffic_under_load` hang): the same on a store of 1,000 changesets as on one of 10,000,
//! and no `git gc --auto` while the lock is held. Counts, not times: work that grows with the
//! history shows as a count that differs, with no clock noise.
//!
//! ```text
//!   built(N), once: N native changesets on main ─▶ serve --prepare (gc off: all loose)
//!   each run: a copy ─▶ client: clone, release/client from origin/main, one commit, push
//!      proc-receive: serve.lock ── export runs, export oids asked, changesets loaded,
//!                                  derived ── let go
//!   phase log:  <judge's totals>  serve.push.held 0 0  <held totals>  serve.push.<work> 0 <n>
//!   then main moves, and a second push: its connection's refresh writes objects, and gc runs
//!
//!   a store branch new to serve.git, one commit past main ─▶ a connection (ls-remote)
//!      its refresh: serve.lock ── exports ── let go
//!   phase log:  serve.refresh.held 0 0  serve.refresh.<work> 0 <n>
//! ```
//!
//! The counts come from the push's hook through the phase log (`SWARMOPS_TEST_PHASE_LOG`, the
//! test hooks), the gc's through a `SWARMOPS_GIT` wrapper that notes, at each `gc`, whether
//! `serve.lock` is free. Before the fix the first push walked every loose commit of `main`
//! under the lock (1,001 changesets loaded against 10,001), and the refresh ran `git gc --auto`
//! holding it.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use camino::Utf8Path;
use swarmops_core::{
    BlobId, ChangesetId, LeafKind, ObjectKind, Signature, Tree, TreeEntry, hash_object,
};
use swarmops_git::serve::repo::ServeRepo;
use swarmops_repo::{LocalRepo, RawChangeset};
use swarmops_testkit::measure::base_dir;
use swarmops_testkit::remote::{GIT_ENV, GitServer};
use swarmops_testkit::serve::ServeSite;
use tempfile::TempDir;

use crate::support;

/// The smaller history.
const SMALL: usize = 1_000;

/// The larger history.
const LARGE: usize = 10_000;

/// The store's name under the root.
const STORE: &str = "repo";

/// The trunk the history is on and the branch forks from.
const MAIN: &str = "main";

/// The changesets one frame of the generator records.
const PART: usize = 2_048;

/// The generator's name and version in a built store's record: a change to what [`built`]
/// makes changes it, and a store built before is built again.
const GENERATOR: &str = "serve-lock 1";

/// The name of a built store's record, written once it is whole.
const MADE: &str = "made.txt";

/// The built store's directory, beside its record.
const BUILT_STORE: &str = "store";

/// The built store's memo key directory, beside it.
const KEYS: &str = "memo-keys";

/// The variable that names the memo keys' directory (the CLI's `MEMO_KEY_DIR_ENV`).
const MEMO_KEY_DIR_ENV: &str = "SWARMOPS_MEMO_KEY_DIR";

/// How long the built store's export may take: minutes at 10,000 commits on a loaded host.
const BUILD_WITHIN: Duration = Duration::from_mins(30);

/// The agent that commits on `main`.
const WRITER: &str = "agent/worker-importer";

/// The person who pushes.
const PUSHER: &str = "person/client";

/// The branch the push makes.
const BRANCH: &str = "release/client";

/// The served store's access file: every person reads and pushes `release/*`.
const ACCESS: &str = r#"version = 1

[[grant]]
who = ["person/*"]
read = true
push = ["release/*"]
"#;

/// The store's file without which nobody is served, and no preparation runs after a move.
const ACCESS_FILE: &str = "access.toml";

/// Where the access file waits while the test writes the store: no detached preparation then.
const ACCESS_ASIDE: &str = "access.toml.aside";

/// The phase log's variable.
const PHASE_LOG_ENV: &str = "SWARMOPS_TEST_PHASE_LOG";

/// The phase log's line before what a push did under `serve.lock`.
const HELD_MARK: &str = "serve.push.held";

/// The wrapper's line for a `gc` run while `serve.lock` was held.
const GC_HELD: &str = "gc held";

/// The store branch only the store holds: `serve.git` has no ref of it until a refresh.
const NEW_BRANCH: &str = "release/new";

/// The phase log's line before what a refresh's exports did under `serve.lock`.
const REFRESH_MARK: &str = "serve.refresh.held";

/// A refresh's held work compared, by the phase log's name.
const REFRESHED: [&str; 3] = [
    "serve.refresh.exports",
    "serve.refresh.export_of",
    "serve.refresh.nodes",
];

/// The held work compared, by the phase log's name.
const COMPARED: [&str; 4] = [
    "serve.push.exports",
    "serve.push.export_of",
    "serve.push.nodes",
    "export.derive",
];

/// One served store of native changesets on `main`, its site counting the hooks' work and
/// their gcs, and a directory for the client.
struct Served {
    /// The site; its children write the phase log and run git through the wrapper.
    site: ServeSite,
    /// The client's clone and the logs.
    work: TempDir,
}

impl Served {
    /// A copy of the store of `changesets` native changesets on `main`, exported into its
    /// `serve.git` ([`built`]), served with the access file in place.
    fn build(changesets: usize) -> Served {
        // 1. The logs and the wrapper, which the site's children get, with the built store's
        //    memo key: the export oids its preparation memoized are trusted.
        let cache = built(changesets);
        let work = TempDir::new().expect("a temp dir");
        let wrapper = write_wrapper(work.path());
        let log = work.path().join("phases.log");
        let keys = cache.join(KEYS);
        let site = ServeSite::new::<support::Swarmops>().with_env(&[
            (PHASE_LOG_ENV, path_str(&log)),
            (GIT_ENV, path_str(&wrapper)),
            (MEMO_KEY_DIR_ENV, path_str(&keys)),
        ]);

        // 2. The built store copied in, then the access file.
        let store = site.store(STORE);
        copy_tree(&cache.join(BUILT_STORE), &store, Link::Copy);
        fs::write(store.join(ACCESS_FILE), ACCESS).expect("write the access file");
        Served { site, work }
    }

    /// git with `args` in `dir` as [`PUSHER`]; it must succeed.
    fn git(&self, dir: &Path, args: &[&str]) {
        let output = self
            .site
            .command()
            .env("GIT_SSH_COMMAND", self.site.ssh_command(PUSHER))
            .current_dir(dir)
            .args(args)
            .output()
            .expect("run git");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "git {args:?}: {stderr}");
    }

    /// The client's clone, on [`BRANCH`] from `origin/main`.
    fn client(&self) -> PathBuf {
        let url = self.site.ssh_url(STORE);
        self.git(self.work.path(), &["clone", "--quiet", &url, "client"]);
        let dir = self.work.path().join("client");
        self.git(&dir, &["checkout", "--quiet", "-b", BRANCH, "origin/main"]);
        dir
    }

    /// One commit of `path` on the client's branch, pushed: the work the push held the lock for.
    fn push(&self, dir: &Path, path: &str) -> BTreeMap<String, u64> {
        fs::write(dir.join(path), format!("{path}\n")).expect("write a file");
        self.git(dir, &["add", path]);
        self.git(dir, &["commit", "--quiet", "-m", path]);
        let _ = fs::remove_file(self.log());
        let refspec = format!("HEAD:refs/heads/{BRANCH}");
        self.git(dir, &["push", "--quiet", "origin", &refspec]);
        held(&self.read_log(), HELD_MARK)
    }

    /// A connection that lists `serve.git`'s refs: the work its refresh held the lock for.
    fn connect(&self) -> BTreeMap<String, u64> {
        let _ = fs::remove_file(self.log());
        let url = self.site.ssh_url(STORE);
        self.git(self.work.path(), &["ls-remote", "--quiet", &url]);
        held(&self.read_log(), REFRESH_MARK)
    }

    /// The phase log's text.
    fn read_log(&self) -> String {
        fs::read_to_string(self.log()).expect("the phase log")
    }

    /// The phase log.
    fn log(&self) -> PathBuf {
        self.work.path().join("phases.log")
    }

    /// The wrapper's lines: one per `gc`, `gc held` or `gc free`.
    fn gcs(&self) -> Vec<String> {
        let text = fs::read_to_string(self.work.path().join("gc.log")).unwrap_or_default();
        text.lines().map(str::to_owned).collect()
    }

    /// The store's path.
    fn store(&self) -> PathBuf {
        self.site.store(STORE)
    }
}

/// The store of `changesets` changesets under `SWARMOPS_BENCH_DIR`, exported into its
/// `serve.git` with git's automatic gc off there (the history stays loose, as between two of
/// a busy store's gcs), with its memo key in [`KEYS`] beside it: built once and reused while
/// its record matches, as the export of 10,000 commits takes minutes. Two tests that need it
/// build it one after the other, under a lock beside it. The directory holding both.
fn built(changesets: usize) -> PathBuf {
    let base = base_dir();
    let dir = base.join(format!("serve-lock-{changesets}"));
    let lock = fs::File::create(base.join(format!("serve-lock-{changesets}.lock")))
        .expect("create the store's lock file");
    lock.lock().expect("lock the store");

    // 1. Whole, by this generator: reused as it is.
    let record = format!("{GENERATOR}\nchangesets {changesets}\n");
    if fs::read_to_string(dir.join(MADE)).is_ok_and(|made| made == record) {
        return dir;
    }

    // 2. Else made again from nothing: the history, serve.git with gc off, the export; the
    //    record last.
    match fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => panic!("remove {}: {e}", dir.display()),
    }
    let store = dir.join(BUILT_STORE);
    fs::create_dir_all(&store).expect("create the store's directory");
    generate(&store, changesets);
    let root = Utf8Path::from_path(&store).expect("a UTF-8 bench path");
    let serve = ServeRepo::open(root).expect("make serve.git");
    let git = GitServer::new();
    let mut config = git.command();
    config.arg("--git-dir").arg(serve.git_dir().as_std_path());
    let output = config
        .args(["config", "gc.auto", "0"])
        .output()
        .expect("run git config");
    assert!(output.status.success(), "git config: {output:?}");
    let keys = dir.join(KEYS);
    let prepare = ["--repo", path_str(&store), "serve", "--prepare"];
    support::run_within(
        &prepare,
        &[(MEMO_KEY_DIR_ENV, path_str(&keys))],
        BUILD_WITHIN,
    )
    .ok();
    fs::write(dir.join(MADE), record).expect("write the record");
    dir
}

/// How [`copy_tree`] makes a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Link {
    /// A hard link: the object files, which neither the store nor git writes into.
    Hard,
    /// A copy of the bytes.
    Copy,
}

/// Copies the directory `from` to `to`; `link` says how its files are made, and everything
/// under a directory named `objects` (the store's, `serve.git`'s) is hard-linked where `to` is
/// on `from`'s file system (the bench directory and the temp directory may not be).
fn copy_tree(from: &Path, to: &Path, link: Link) {
    fs::create_dir_all(to).expect("create a directory of the copy");
    for entry in fs::read_dir(from).expect("read a directory of the store") {
        let entry = entry.expect("a directory entry");
        let target = to.join(entry.file_name());
        let kind = entry.file_type().expect("an entry's type");

        // A directory: its files hard-linked from `objects/` down.
        if kind.is_dir() {
            let below = if entry.file_name() == "objects" {
                Link::Hard
            } else {
                link
            };
            copy_tree(&entry.path(), &target, below);
            continue;
        }

        // A file: linked where the copy is on the same file system, else copied.
        let linked = link == Link::Hard && fs::hard_link(entry.path(), &target).is_ok();
        if !linked {
            fs::copy(entry.path(), &target).expect("copy a file of the store");
        }
    }
}

/// Makes at `store` a store of `changesets` native changesets on `main`, each changing
/// `hist.txt` (two roots in turn), recorded a part at a time.
fn generate(store: &Path, changesets: usize) {
    let root = Utf8Path::from_path(store).expect("a UTF-8 path");
    let repo = LocalRepo::create(root, STORE).expect("create the store");

    // 1. Two roots that differ in `hist.txt` alone.
    let mut objects = Vec::new();
    let roots = [0, 1].map(|turn| {
        let hist = format!("turn {turn}\n").into_bytes();
        let hist_id = BlobId::new(hash_object(ObjectKind::Blob, &hist));
        objects.push((ObjectKind::Blob, hist));
        let tree = Tree::new(vec![TreeEntry::leaf("hist.txt", LeafKind::File, hist_id)])
            .expect("a valid root");
        objects.push((ObjectKind::Tree, tree.encode()));
        tree.id()
    });
    repo.put_objects(objects).expect("put the objects");

    // 2. One chain, a frame a part.
    let mut parent: Option<ChangesetId> = None;
    for part in (0..changesets).collect::<Vec<_>>().chunks(PART) {
        let mut batch = repo.raw_batch();
        for &k in part {
            let mut raw = RawChangeset::new(
                Vec::new(),
                roots[k % 2],
                signature(k),
                signature(k),
                format!("native {k}").into_bytes(),
                Vec::new(),
                Vec::new(),
            );
            raw.parents = parent.into_iter().collect();
            parent = Some(batch.push(raw).expect("push a changeset"));
        }
        batch.commit().expect("record the part");
    }
    repo.cas_ref(MAIN, None, parent, "serve-lock")
        .expect("move main");
    repo.flush_metadata().expect("flush the metadata");
}

/// A fixed signature for changeset `k`.
fn signature(k: usize) -> Signature {
    let secs = 1_760_000_000 + i64::try_from(k).expect("a changeset index fits");
    Signature::new(
        b"serve-lock/agent".to_vec(),
        b"agent@serve-lock.swarmops.local".to_vec(),
        secs,
        0,
        false,
    )
    .expect("a zero offset is valid")
}

/// `path` as text.
fn path_str(path: &Path) -> &str {
    path.to_str().expect("UTF-8 paths")
}

/// A commit of `file` on `main` of the store at `store` as [`WRITER`], its change list written
/// in `work`, with the access file aside: no detached preparation runs beside the test's
/// connections.
fn commit_on_main(store: &Path, work: &Path, file: &str) {
    let access = store.join(ACCESS_FILE);
    let aside = store.join(ACCESS_ASIDE);
    let had = access.is_file();
    if had {
        fs::rename(&access, &aside).expect("set the access file aside");
    }
    let changes = work.join(format!("{file}.json"));
    let list = format!(r#"[{{"op":"put","path":"{file}","content":{{"inline":"{file}\n"}}}}]"#);
    fs::write(&changes, list).expect("write a change list");
    let args = [
        "--repo",
        path_str(store),
        "--as",
        WRITER,
        "commit",
        "-m",
        file,
        "--changes",
        path_str(&changes),
        "--advance",
        "main",
    ];
    support::run(&args, &[]).ok();
    if had {
        fs::rename(&aside, &access).expect("put the access file back");
    }
}

/// A `git` that appends `gc held` or `gc free` to `gc.log` beside it at each `gc`, whether
/// `serve.lock` (beside `GIT_DIR`) could be taken at once, then runs the real one.
fn write_wrapper(dir: &Path) -> PathBuf {
    let real = std::env::var(GIT_ENV).unwrap_or_else(|_| "git".to_owned());
    let log = dir.join("gc.log");
    let script = format!(
        "#!/bin/sh\n\
         case \" $* \" in *\" gc \"*)\n\
         \x20 if flock -n \"$GIT_DIR/../serve.lock\" true; then echo 'gc free' >> '{log}'; \
         else echo '{GC_HELD}' >> '{log}'; fi;;\n\
         esac\n\
         exec '{real}' \"$@\"\n",
        log = path_str(&log),
    );
    let wrapper = dir.join("git-wrapper");
    fs::write(&wrapper, script).expect("write the wrapper");
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).expect("make it runnable");
    wrapper
}

/// The counts of `log`'s last held section, by name: the lines after its last `mark`.
fn held(log: &str, mark: &str) -> BTreeMap<String, u64> {
    let lines: Vec<&str> = log.lines().collect();
    let start = (lines.iter())
        .rposition(|line| line.starts_with(mark))
        .unwrap_or_else(|| panic!("no {mark} in the phase log"));
    let mut counts = BTreeMap::new();
    for line in &lines[start + 1..] {
        let mut fields = line.split(' ');
        let (Some(name), Some(_), Some(count)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let count: u64 = count.parse().expect("a count");
        *counts.entry(name.to_owned()).or_default() += count;
    }
    counts
}

/// The counts of `counts` named in `names`, zero where missing.
fn compared(counts: &BTreeMap<String, u64>, names: &[&'static str]) -> Vec<(&'static str, u64)> {
    (names.iter())
        .map(|name| (*name, counts.get(*name).copied().unwrap_or(0)))
        .collect()
}

/// The compared counts of one push, by name.
type Counted = Vec<(&'static str, u64)>;

/// A push's held work on a store of `changesets`: the first push of a new branch, then a
/// second push after `main` moved; and the wrapper's gc lines.
fn pushes(changesets: usize) -> (Counted, Counted, Vec<String>) {
    let served = Served::build(changesets);
    let client = served.client();
    let first = served.push(&client, "one.txt");
    commit_on_main(&served.store(), served.work.path(), "moved.txt");
    let second = served.push(&client, "two.txt");
    eprintln!("serve-lock: changesets={changesets} first={first:?} second={second:?}");
    (
        compared(&first, &COMPARED),
        compared(&second, &COMPARED),
        served.gcs(),
    )
}

#[test]
fn a_push_holds_serve_lock_for_work_its_history_does_not_grow() {
    let (small_first, small_second, small_gcs) = pushes(SMALL);
    let (large_first, large_second, large_gcs) = pushes(LARGE);

    // 1. The held work, the same on both histories.
    assert_eq!(small_first, large_first, "a new branch's push");
    assert_eq!(small_second, large_second, "a moved branch's push");

    // 2. No gc while serve.lock is held, and the gc ran (the refresh wrote objects).
    for gcs in [&small_gcs, &large_gcs] {
        assert!(!gcs.is_empty(), "no gc ran");
        assert!(
            !gcs.iter().any(|line| line == GC_HELD),
            "a gc ran under serve.lock: {gcs:?}"
        );
    }
}

/// Makes [`NEW_BRANCH`] in the store at `store`: one changeset on `main`'s head, its root
/// unchanged. Written straight into the store, as no connection runs meanwhile.
fn branch_off_main(store: &Path) {
    let root = Utf8Path::from_path(store).expect("a UTF-8 path");
    let repo = LocalRepo::open(root).expect("open the store");
    let main = (repo.get_ref(MAIN).expect("read main"))
        .expect("main exists")
        .target;
    let parent = (repo.changeset(main).expect("read main's head"))
        .expect("main's head is stored")
        .changeset;
    let raw = RawChangeset::new(
        vec![main],
        parent.root,
        signature(0),
        signature(0),
        b"new branch".to_vec(),
        Vec::new(),
        Vec::new(),
    );
    let mut batch = repo.raw_batch();
    let head = batch.push(raw).expect("push a changeset");
    batch.commit().expect("record it");
    repo.cas_ref(NEW_BRANCH, None, Some(head), "serve-lock")
        .expect("make the branch");
    repo.flush_metadata().expect("flush the metadata");
}

/// A connection's refresh's held work on a store of `changesets`, once the store has a branch
/// `serve.git` does not hold.
fn refreshes(changesets: usize) -> Counted {
    let served = Served::build(changesets);
    branch_off_main(&served.store());
    let counts = served.connect();
    eprintln!("serve-lock: changesets={changesets} refresh={counts:?}");
    compared(&counts, &REFRESHED)
}

#[test]
fn a_refresh_holds_serve_lock_for_work_its_history_does_not_grow() {
    let small = refreshes(SMALL);
    let large = refreshes(LARGE);
    assert_eq!(small, large, "a new branch's refresh");
}
