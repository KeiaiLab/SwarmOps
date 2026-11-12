//! The store the seeded `serve` tests start from: [`COMMITS`] commits of `serve_perf`'s history,
//! brought in by one `git import` of the binary under test, once for a build of that binary.
//!
//! Four tests of `just check` assert what `serve --prepare` and the first clone write on such a
//! store. Each ran the import itself, 200 durable commits in one call, beside the other three:
//! on a loaded host one of those calls passed the runner's limit for a call. The import now runs
//! once, under a lock, and each test takes a copy; the one test that reads what the import
//! itself starts (`serve_perf::seeded_bench_waits_for_its_preparation`) imports under the same
//! lock ([`turn`]), so no import runs beside another.
//!
//! ```text
//!   <target>/tmp/seeded-200/        (CARGO_TARGET_TMPDIR, one per target directory)
//!     ../seeded-200.lock            one import, or one copy, at a time
//!     store/                        the imported store: copied from, never written again
//!     keys/<repo_id>.key            the memo key the import signed under
//!     made.txt                      what made it, written last
//! ```

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use swarmops_testkit::remote::GitServer;

use crate::support;

/// The commits of the imported history.
pub(crate) const COMMITS: usize = 200;

/// The version of [`import`]: a change of what it makes takes a new one, so that a store made
/// before is made again.
const GENERATOR: &str = "seeded 1";

/// The name of the store's record, written once it is whole: the generator, the binary that
/// imported and the size.
const MADE: &str = "made.txt";

/// The imported store, and the copy of its memo key, in the fixture's directory.
const STORE: &str = "store";
const KEYS: &str = "keys";

/// What a memo key's file name ends in (`<repo_id>.key`).
const KEY_SUFFIX: &str = ".key";

/// The variable that names the memo keys' directory (the CLI's `context::MEMO_KEY_DIR_ENV`,
/// which a test cannot name): the test kit sets it on every run of the binary.
const MEMO_KEY_DIR_ENV: &str = "SWARMOPS_MEMO_KEY_DIR";

/// Who imports.
const WRITER: &str = "agent/worker-1";

/// One import's turn: held while the store is made or copied, let go when dropped.
pub(crate) struct Turn {
    /// The lock file, locked.
    _lock: File,
}

/// The fixture's directory.
fn dir() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("seeded-{COMMITS}"))
}

/// Waits for the turn to import [`COMMITS`] commits, and takes it. A test that runs the import
/// itself holds it meanwhile, and must not ask for the store before letting go.
pub(crate) fn turn() -> Turn {
    let dir = dir();
    let base = dir.parent().expect("a directory under the target's");
    fs::create_dir_all(base).expect("create the target's temp directory");
    let lock = File::create(base.join(format!("seeded-{COMMITS}.lock")))
        .expect("create the store's lock file");
    lock.lock().expect("lock the store");
    Turn { _lock: lock }
}

/// The directory every run of the binary reads its memo keys from, off the command the kit
/// builds.
fn memo_keys() -> PathBuf {
    let command = support::swarmops::<&str>(&[], &[]);
    let dir = command
        .get_envs()
        .find(|(name, _)| *name == MEMO_KEY_DIR_ENV)
        .and_then(|(_, value)| value)
        .expect("the kit names the memo keys' directory");
    PathBuf::from(dir)
}

/// What a store made now would record: the generator, the binary under test as it is on disk
/// now, and the size.
fn record() -> String {
    let bin = fs::metadata(support::BIN).expect("the binary under test");
    let modified = bin.modified().expect("the binary's time");
    let since = modified
        .duration_since(UNIX_EPOCH)
        .expect("a time past 1970");
    format!(
        "{GENERATOR}\nbinary {}\nlength {}\nmodified {}\ncommits {COMMITS}\n",
        support::BIN,
        bin.len(),
        since.as_nanos()
    )
}

/// Whether every key under `keys` is still the one the runs read: a store's memos are signed
/// under its key, and the keys' directory is the temp directory's, which outlives no cleanup.
fn keys_kept(keys: &Path) -> bool {
    let Ok(entries) = fs::read_dir(keys) else {
        return false;
    };
    let live = memo_keys();
    entries.map(|entry| entry.expect("a key")).all(|entry| {
        let kept = fs::read(entry.path()).expect("read the kept key");
        fs::read(live.join(entry.file_name())).is_ok_and(|key| key == kept)
    })
}

/// The imported store, made if no whole one of this binary is there: under `turn`.
fn built(_turn: &Turn) -> PathBuf {
    let dir = dir();
    let store = dir.join(STORE);

    // 1. Whole, by this generator and this binary, its key still read: reused as it is.
    let record = record();
    let whole = fs::read_to_string(dir.join(MADE)).is_ok_and(|made| made == record);
    if whole && keys_kept(&dir.join(KEYS)) {
        return store;
    }

    // 2. Else made again from nothing, the record written last.
    match fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => panic!("remove {}: {e}", dir.display()),
    }
    fs::create_dir_all(dir.join(KEYS)).expect("create the store's directory");
    let repo_id = import(&store);
    let key = format!("{repo_id}{KEY_SUFFIX}");
    if let Ok(bytes) = fs::read(memo_keys().join(&key)) {
        fs::write(dir.join(KEYS).join(&key), bytes).expect("keep the memo key");
    }
    fs::write(dir.join(MADE), record).expect("write the record");
    store
}

/// Makes the store at `root`: `init`, then one `git import` of `serve_perf`'s history into
/// `main`, with no access file, so that the import starts no preparation. The store's id.
fn import(root: &Path) -> String {
    // 1. The history, in a bare repository that goes with this call.
    let server = GitServer::new();
    let source = server.create("source");
    crate::serve_perf::fast_import(&server, &source, COMMITS);

    // 2. The store, and the import: one call, inside the runner's limit for a call.
    let (root, source) = (path_str(root), path_str(&source));
    let made = support::run(&["init", root], &[]).ok();
    let repo_id = made["repo_id"].as_str().expect("a store id").to_owned();
    let line = [
        "--repo", root, "--as", WRITER, "git", "import", source, "--ref", "main", "--into", "main",
    ];
    support::run(&line, &[]).ok();
    repo_id
}

/// `path` as text.
fn path_str(path: &Path) -> &str {
    path.to_str().expect("UTF-8 paths")
}

/// Copies the directory `from` to `to`.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create a directory of the copy");
    for entry in fs::read_dir(from).expect("read a directory of the store") {
        let entry = entry.expect("a directory entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("an entry's type").is_dir() {
            copy_tree(&entry.path(), &target);
            continue;
        }
        fs::copy(entry.path(), &target).expect("copy a file of the store");
    }
}

/// Puts a copy of the imported store at `root`, in place of whatever is there: a store of the
/// test's own, with the imported one's id, memo key and `main`, and no access file.
pub(crate) fn put(root: &Path) {
    let turn = turn();
    let store = built(&turn);
    match fs::remove_dir_all(root) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => panic!("remove {}: {e}", root.display()),
    }
    copy_tree(&store, root);
}

/// The imported store itself, for a test that reads what the import left in it. No test writes
/// into it.
pub(crate) fn imported() -> PathBuf {
    built(&turn())
}

/// `info` of the store at `root`.
fn info(root: &Path) -> serde_json::Value {
    support::run(&["--repo", path_str(root), "info"], &[]).ok()
}

/// A copy is the imported store, and a store of its own: it holds [`COMMITS`] changesets under
/// the imported one's id, and a change of it reaches neither the imported store nor a second
/// copy.
#[test]
fn a_copy_is_a_store_of_its_own() {
    let work = tempfile::tempdir().expect("a temp dir");
    let (first, second) = (work.path().join("first"), work.path().join("second"));
    put(&first);
    let before = info(&imported());
    assert_eq!(before["changesets"], COMMITS, "{before}");
    assert_eq!(info(&first), before);

    // 1. One changeset more in the first copy.
    let changes = work.path().join("change.json");
    let list = r#"[{"path": "own.txt", "op": "put", "content": {"inline": "own\n"}}]"#;
    fs::write(&changes, list).expect("write the change");
    let line = [
        "--repo",
        path_str(&first),
        "--as",
        WRITER,
        "commit",
        "-m",
        "own",
        "--changes",
        path_str(&changes),
        "--parent",
        "main",
    ];
    support::run(&line, &[]).ok();
    assert_eq!(info(&first)["changesets"], COMMITS + 1);

    // 2. Neither the imported store nor a copy made after it holds it.
    put(&second);
    assert_eq!(info(&imported()), before);
    assert_eq!(info(&second), before);
}

/// A store made by another binary, or by another generator, is made again: the record names
/// both, and a record that differs in either is not this one.
#[test]
fn the_record_names_the_binary_and_the_generator() {
    let record = record();
    assert!(record.starts_with(&format!("{GENERATOR}\n")), "{record}");
    assert!(
        record.contains(&format!("\nbinary {}\n", support::BIN)),
        "{record}"
    );
    let length = fs::metadata(support::BIN).expect("the binary").len();
    assert!(record.contains(&format!("\nlength {length}\n")), "{record}");
}
