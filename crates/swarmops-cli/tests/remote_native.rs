//! `so://` remotes (the native sync design §2.1, §2.4, §6 N-4): a host store clones and
//! fetches a served store over native sync's wire, through the test kit's ssh stand-in that
//! runs `swarmops serve` as the key's forced command. Changesets cross as their own bytes, so a
//! host holds the upstream's ids; the tracking refs `remote/<remote>/<ref>` move last.
//!
//! ```text
//!   host store ── swarmops clone|fetch so://ada@hub.example/acme
//!        └── $GIT_SSH_COMMAND (stand-in, key of person/ada) ── swarmops serve --stdio 'acme'
//!                                                                └── the served store
//! ```
//!
//! Wrappers of the stand-in show what crossed: one keeps every byte the upstream answered (its
//! envelope lines and chunks), one damages the answer, one answers as a server older than native
//! sync does.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
#[cfg(feature = "test-hooks")]
use swarmops_core::{ObjectId, ObjectKind, hash_object};
#[cfg(feature = "test-hooks")]
use swarmops_store::{CRASH_AT_ENV, FS_CALLS_ENV};
use swarmops_testkit::git::{self, fixture};
use swarmops_testkit::remote::GitServer;
use swarmops_testkit::serve::ServeSite;
use tempfile::TempDir;

#[cfg(feature = "test-hooks")]
use crate::support::sweep::reported_calls;
use crate::support::{Run, Store, Swarmops, run};

/// The agent a host's commands run as.
const AGENT: &str = "agent/worker-17";

/// The agent that writes the served store directly.
const WRITER: &str = "agent/worker-1";

/// The identity of the host's key.
const ADA: &str = "person/ada";

/// The served store.
const STORE: &str = "acme";

/// The served store's URL; the stand-in drops the host.
const URL: &str = "so://ada@hub.example/acme";

/// The remote's name in a host store: the clone's default.
const ORIGIN: &str = "origin";

/// The tracking ref of the upstream's `main`.
const TRACKING_MAIN: &str = "remote/origin/main";

/// The variable that names the git the binary runs.
const GIT_VAR: &str = "SWARMOPS_GIT";

/// The variable git, and native sync, take the ssh command from.
const SSH_VAR: &str = "GIT_SSH_COMMAND";

/// The served store's grants: ada reads.
const ACCESS: &str = r#"version = 1

[[grant]]
who = ["person/ada"]
read = true
"#;

/// The most changesets a log lists here.
const LOG_LIMIT: &str = "1000";

#[cfg(feature = "test-hooks")]
/// The size of each large file the crash sweep fetches.
const LARGE: usize = 96 * 1024;

/// What a server older than native sync answers `swarmops serve --stdio` with (the probe's
/// step 00b): git's `ERR` pkt-line.
const OLD_SERVER_SAYS: &str =
    "ERR E_INVALID_ARG: swarmops serve --stdio, store-to-store sync, is not served yet";

#[cfg(feature = "test-hooks")]
/// A chunk's header: `SWCH`, a version byte, three zeros, the count (u32) and bytes (u64).
const CHUNK_HEADER: usize = 20;

#[cfg(feature = "test-hooks")]
/// A chunk record's header: kind, id, payload length (u32).
const RECORD_HEADER: usize = 37;

/// A served store, the host's git environment, and a place for wrappers and change lists.
struct Site {
    served: ServeSite,
    git: GitServer,
    home: TempDir,
    work: TempDir,
    files: AtomicU64,
}

impl Site {
    /// The served store with one change on `main`, ada granted read.
    fn new() -> Site {
        let site = Site::bare();
        site.land("README.md", "hello\n");
        site
    }

    /// The served store with no ref.
    fn bare() -> Site {
        let served = ServeSite::new::<Swarmops>();
        served.init(STORE);
        let site = Site {
            served,
            git: GitServer::new(),
            home: tempfile::tempdir().expect("a temp HOME"),
            work: tempfile::tempdir().expect("a temp dir"),
            files: AtomicU64::new(0),
        };
        site.grant();
        site
    }

    /// Writes the served store's `access.toml`.
    fn grant(&self) {
        fs::write(self.served.store(STORE).join("access.toml"), ACCESS).expect("write access");
    }

    /// The served store's path.
    fn store(&self) -> String {
        path_text(&self.served.store(STORE))
    }

    /// `args` on the served store as [`WRITER`], which must succeed.
    fn so(&self, args: &[&str]) -> Value {
        let store = self.store();
        let mut line = vec!["--repo", store.as_str(), "--as", WRITER];
        line.extend(args);
        run(&line, &[]).ok()
    }

    /// A new file in the work directory holding `content`.
    fn file(&self, content: &[u8]) -> String {
        let n = self.files.fetch_add(1, Ordering::Relaxed);
        let file = self.work.path().join(format!("file-{n}"));
        fs::write(&file, content).expect("write a file");
        path_text(&file)
    }

    /// A changeset on `parent` (none: a root) with the changes `changes` (a JSON list), which no
    /// ref names yet.
    fn commit_changes(&self, parent: Option<&str>, changes: &Value) -> String {
        let file = self.file(changes.to_string().as_bytes());
        let mut line = vec!["commit", "-m", "change", "--changes", &file];
        if let Some(parent) = parent {
            line.extend(["--parent", parent]);
        }
        text(&self.so(&line)["changeset"])
    }

    /// A changeset on `main` putting `content` at `path`, `main` moved to it.
    fn land(&self, path: &str, content: &str) -> String {
        let changes =
            serde_json::json!([{"path": path, "op": "put", "content": {"inline": content}}]);
        self.advance("main", &changes)
    }

    /// A changeset on `branch` (created at `main` if absent) with `changes`, the branch moved to
    /// it.
    fn advance(&self, branch: &str, changes: &Value) -> String {
        let old = self.ref_at(branch);
        let new = self.commit_changes(old.as_deref(), changes);
        match old {
            Some(old) => self.so(&["ref", "set", branch, &new, "--expect", &old]),
            None => self.so(&["ref", "set", branch, &new, "--create"]),
        };
        new
    }

    /// The changeset the served store's `name` names, if it exists.
    fn ref_at(&self, name: &str) -> Option<String> {
        ref_in(&self.store(), name)
    }

    /// The served `main`'s head.
    fn main_head(&self) -> String {
        self.ref_at("main").expect("the served main")
    }

    /// The variables a host's run sets, its ssh command `ssh`.
    fn env(&self, ssh: &str) -> Vec<(String, String)> {
        vec![
            (GIT_VAR.to_owned(), path_text(&real_git(&self.git))),
            ("HOME".to_owned(), path_text(self.home.path())),
            ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
            ("GIT_CONFIG_GLOBAL".to_owned(), "/dev/null".to_owned()),
            (SSH_VAR.to_owned(), ssh.to_owned()),
        ]
    }

    /// The stand-in of ada's key.
    fn ssh(&self) -> String {
        self.served.ssh_command(ADA)
    }

    /// A stand-in of ada's key whose answer passes through `filter` (a shell pipeline stage).
    fn ssh_through(&self, filter: &str) -> String {
        let n = self.files.fetch_add(1, Ordering::Relaxed);
        let script = self.work.path().join(format!("ssh-wrapper-{n}"));
        let body = format!("#!/bin/sh\n{} \"$@\" | {filter}\n", self.ssh());
        write_script(&script, &body);
        path_text(&script)
    }

    /// A stand-in of ada's key that keeps every byte the upstream answers in `log`.
    fn ssh_logged(&self, log: &Path) -> String {
        self.ssh_through(&format!("tee -a '{}'", path_text(log)))
    }

    /// `args` on `host` with the ssh command `ssh`.
    fn on(&self, host: &str, ssh: &str, args: &[&str], extra: &[(&str, &str)]) -> Run {
        let env = self.env(ssh);
        let mut vars: Vec<(&str, &str)> =
            env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        vars.extend(extra);
        let mut line = vec!["--repo", host, "--as", AGENT];
        line.extend(args);
        run(&line, &vars)
    }

    /// `fetch origin` on `host` through `ssh`.
    fn fetch(&self, host: &str, ssh: &str) -> Run {
        self.on(host, ssh, &["fetch", ORIGIN], &[])
    }

    /// A host cloned from [`URL`] through `ssh`, and the clone's answer.
    fn clone_through(&self, ssh: &str) -> (Store, Value) {
        let store = Store::empty();
        let env = self.env(ssh);
        let vars: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let line = ["--as", AGENT, "clone", URL, "--into", store.path()];
        let cloned = run(&line, &vars).ok();
        (store, cloned)
    }

    /// A host cloned from [`URL`].
    fn cloned(&self) -> (Store, Value) {
        self.clone_through(&self.ssh())
    }
}

/// The ids `rev`'s history lists in the store at `store`, newest first, each with its parents.
fn history(store: &str, rev: &str) -> Vec<(String, Vec<String>)> {
    let log = run(&["--repo", store, "log", rev, "--limit", LOG_LIMIT], &[]).ok();
    assert_eq!(log["truncated"], false, "{log}");
    let entries = log["entries"].as_array().expect("entries");
    let entry = |e: &Value| {
        let parents = e["parents"].as_array().expect("parents");
        (text(&e["id"]), parents.iter().map(text).collect())
    };
    entries.iter().map(entry).collect()
}

/// The changeset ref `name` names in the store at `store`, if it exists.
fn ref_in(store: &str, name: &str) -> Option<String> {
    let refs = run(&["--repo", store, "ref", "list"], &[]).ok();
    let listed = refs["refs"].as_array().expect("refs");
    let found = listed.iter().find(|entry| entry["name"] == name)?;
    Some(text(&found["target"]))
}

/// Asserts that `verify --deep` finds nothing in the store at `store`.
fn assert_clean(store: &str) {
    let verify = run(&["--repo", store, "verify", "--deep"], &[]);
    assert_eq!(verify.exit(), 0, "{verify}");
}

/// The ids of every object the chunks in `log`, a stand-in's record of the upstream's
/// answers, carried.
#[cfg(feature = "test-hooks")]
fn received(log: &Path) -> Vec<ObjectId> {
    let bytes = fs::read(log).unwrap_or_default();
    let mut ids = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        // 1. An envelope line, and the chunks its result announces.
        let end = at
            + bytes[at..]
                .iter()
                .position(|&b| b == b'\n')
                .expect("a whole line");
        let line: Value = serde_json::from_slice(&bytes[at..end]).expect("an envelope");
        at = end + 1;
        let chunks = line["result"]["chunks"].as_u64().unwrap_or(0);

        // 2. Each chunk's records.
        for _ in 0..chunks {
            let count = u32_at(&bytes, at + 8);
            at += CHUNK_HEADER;
            for _ in 0..count {
                let id: [u8; 32] = bytes[at + 1..at + 33].try_into().expect("an id");
                let len = u32_at(&bytes, at + 33) as usize;
                ids.push(ObjectId::new(id));
                at += RECORD_HEADER + len;
            }
        }
    }
    ids
}

#[cfg(feature = "test-hooks")]
/// The little-endian u32 at `at` of `bytes`.
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

/// How many answers in `log` are `op`'s.
fn answers_of(log: &Path, op: &str) -> usize {
    let bytes = fs::read(log).unwrap_or_default();
    let needle = format!("\"op\":\"{op}\"");
    String::from_utf8_lossy(&bytes).matches(&needle).count()
}

/// The git `SWARMOPS_GIT` names: the stand-in server's own, as an absolute path.
fn real_git(server: &GitServer) -> PathBuf {
    let program = server.git_program();
    if program.is_absolute() {
        return program.to_path_buf();
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
        .expect("git on PATH")
}

/// Writes an executable script.
fn write_script(path: &Path, body: &str) {
    fs::write(path, body).expect("write a script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("make it executable");
    }
}

/// `path` as UTF-8.
fn path_text(path: &Path) -> String {
    path.to_str().expect("a UTF-8 path").to_owned()
}

/// A JSON string's text.
fn text(value: &Value) -> String {
    value.as_str().expect("a string").to_owned()
}

/// A clone holds the upstream's changesets under their own ids, `main` at the upstream's head,
/// its history whole; its git ids are those the git wire answers with.
#[test]
fn clone_natively_keeps_every_id() {
    let site = Site::new();
    site.land("src/lib.rs", "pub fn f() {}\n");
    site.land("src/lib.rs", "pub fn f() -> u8 { 1 }\n");
    let head = site.main_head();

    // 1. The clone.
    let (host, cloned) = site.cloned();
    assert_eq!(cloned["main"], head.as_str(), "{cloned}");
    assert_eq!(
        cloned["fetch"]["head"]["changeset"],
        head.as_str(),
        "{cloned}"
    );
    assert_eq!(cloned["fetch"]["imported"], 3, "{cloned}");
    assert_eq!(cloned["imported"], 3, "{cloned}");

    // 2. The same ids, parents and all, and a clean store.
    assert_eq!(history(host.path(), "main"), history(&site.store(), "main"));
    assert_eq!(ref_in(host.path(), TRACKING_MAIN), Some(head));
    assert_clean(host.path());

    // 3. The git wire answers with the same git id for the same head (decision 12).
    let over_git = Store::empty();
    let env = site.env(&site.ssh());
    let vars: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let url = site.served.ssh_url(STORE);
    let line = [
        "--as",
        AGENT,
        "clone",
        url.as_str(),
        "--into",
        over_git.path(),
    ];
    let git_clone = run(&line, &vars).ok();
    assert_eq!(
        cloned["fetch"]["head"]["sha"],
        git_clone["fetch"]["head"]["sha"]
    );
}

/// A fetch adopts what landed since, with its ids, and moves the tracking ref, not `main`.
#[test]
fn fetch_brings_new_lands_with_their_ids() {
    let site = Site::new();
    let (host, _) = site.cloned();
    let old = site.main_head();
    site.land("a.txt", "a\n");
    let new = site.land("b.txt", "b\n");

    let fetched = site.fetch(host.path(), &site.ssh()).ok();
    assert_eq!(fetched["imported"], 2, "{fetched}");
    assert_eq!(fetched["head"]["changeset"], new.as_str(), "{fetched}");
    assert_eq!(fetched["old_head"]["changeset"], old.as_str(), "{fetched}");
    let moved = fetched["branches"].as_array().expect("branches");
    assert_eq!(moved.len(), 1, "{fetched}");
    assert_eq!(moved[0]["branch"], "main");
    assert_eq!(moved[0]["old"], fetched["old_head"]["sha"]);
    assert_eq!(moved[0]["new"], fetched["head"]["sha"]);

    assert_eq!(ref_in(host.path(), TRACKING_MAIN), Some(new));
    assert_eq!(ref_in(host.path(), "main"), Some(old));
    assert_eq!(
        history(host.path(), TRACKING_MAIN),
        history(&site.store(), "main")
    );
    assert_clean(host.path());
}

/// A fetch with nothing new moves no ref, writes no frame and asks for no object.
#[test]
fn fetch_again_moves_nothing_and_sends_nothing() {
    let site = Site::new();
    let (host, _) = site.cloned();
    let journal = host.journal_len();
    let log = site.work.path().join("answers");

    let fetched = site.fetch(host.path(), &site.ssh_logged(&log)).ok();
    assert_eq!(fetched["imported"], 0, "{fetched}");
    assert_eq!(fetched["branches"], serde_json::json!([]), "{fetched}");
    assert_eq!(fetched["tags_fetched"], 0, "{fetched}");
    assert_eq!(host.journal_len(), journal);
    assert_eq!(answers_of(&log, "wire_get"), 0);
    assert_eq!(answers_of(&log, "wire_hello"), 1);
}

/// Every branch the upstream lists gets its tracking ref, at its id: `release/*`, a branch off
/// `main` with changesets of its own.
#[test]
fn fetch_tracks_every_readable_branch() {
    let site = Site::new();
    let head = site.main_head();
    site.so(&["ref", "set", "release/1", &head, "--create"]);
    let own_change =
        serde_json::json!([{"path": "side.txt", "op": "put", "content": {"inline": "s\n"}}]);
    site.so(&["ref", "set", "feature/a", &head, "--create"]);
    let feature = site.advance("feature/a", &own_change);

    let (host, cloned) = site.cloned();
    let moved = cloned["fetch"]["branches"].as_array().expect("branches");
    assert_eq!(moved.len(), 3, "{cloned}");
    for (name, target) in [
        ("main", &head),
        ("release/1", &head),
        ("feature/a", &feature),
    ] {
        let tracking = format!("remote/{ORIGIN}/{name}");
        assert_eq!(
            ref_in(host.path(), &tracking).as_ref(),
            Some(target),
            "{tracking}"
        );
    }
    assert_eq!(
        history(host.path(), "remote/origin/feature/a"),
        history(&site.store(), "feature/a")
    );
    assert_clean(host.path());
}

/// A fetch cut by a crash at any of its store calls, run again, ends where an uncut one does
/// and asks for no object the host stored before the cut: one stored and not yet adopted is
/// read back (H1), one adopted is held, and a tracking ref left behind moves (H2).
#[cfg(feature = "test-hooks")]
#[test]
fn a_cut_fetch_resumes_and_fetches_no_object_twice() {
    crash_sweep(Step::Coarse);
}

/// [`a_cut_fetch_resumes_and_fetches_no_object_twice`] at every store call.
#[cfg(feature = "test-hooks")]
#[test]
#[ignore = "slow, ~70 s: a fetch crashed at each of its store calls, then resumed"]
fn a_cut_fetch_resumes_and_fetches_no_object_twice_at_every_call() {
    crash_sweep(Step::Every);
}

/// Which store calls the crash sweep crashes at.
#[cfg(feature = "test-hooks")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    /// About twelve spread over the fetch, and each of its last calls.
    Coarse,
    /// Each one.
    Every,
}

/// The calls at the end of a fetch the coarse sweep crashes at each of: the adopt's frame, the
/// tracking refs' moves.
#[cfg(feature = "test-hooks")]
const TAIL_CALLS: u64 = 40;

/// The crash sweep of [`a_cut_fetch_resumes_and_fetches_no_object_twice`].
#[cfg(feature = "test-hooks")]
fn crash_sweep(step: Step) {
    // 1. A host cloned, then three large files landed upstream: what the fetch brings.
    let site = Site::new();
    let (template, _) = site.cloned();
    let mut wanted = Vec::new();
    for n in 0..3_u8 {
        let content: String = (0..LARGE)
            .map(|i| char::from(b'a' + u8::try_from((i ^ usize::from(n)) % 26).unwrap()))
            .collect();
        wanted.push(hash_object(ObjectKind::Blob, content.as_bytes()));
        let id = site.land(&format!("large-{n}.txt"), &content);
        wanted.push(id.parse().expect("an id"));
    }
    let head = site.main_head();
    let changesets: Vec<String> = history(&site.store(), "main")
        .into_iter()
        .take(3)
        .map(|(id, _)| id)
        .collect();

    // 2. How many store calls an uncut fetch makes.
    let dry = Store::copy_of(&template);
    let calls_file = dry.tmp().join("fs-calls");
    let calls_text = path_text(&calls_file);
    site.on(
        dry.path(),
        &site.ssh(),
        &["fetch", ORIGIN],
        &[(FS_CALLS_ENV, &calls_text)],
    )
    .ok();
    let calls = reported_calls(&calls_file);

    // 3. Each crash, then the fetch again through a recording stand-in.
    let crashes: Vec<u64> = match step {
        Step::Every => (1..=calls).collect(),
        Step::Coarse => {
            let every = (calls / 12).max(1);
            let tail = calls.saturating_sub(TAIL_CALLS);
            (1..=calls)
                .filter(|k| k % every == 0 || *k > tail)
                .collect()
        }
    };
    let (mut objects_only, mut tracking_behind) = (false, false);
    for k in crashes {
        let host = Store::copy_of(&template);
        let at = k.to_string();
        let cut_run = site.on(
            host.path(),
            &site.ssh(),
            &["fetch", ORIGIN],
            &[(CRASH_AT_ENV, &at)],
        );
        assert!(cut_run.aborted(), "crash at {k} of {calls}: {cut_run}");

        // What the crash left: objects stored, changesets adopted, the tracking ref.
        let layout = host.layout();
        let stored: HashSet<ObjectId> = (wanted.iter().copied())
            .filter(|id| layout.object_path(*id).exists())
            .collect();
        let adopted = (changesets.iter())
            .filter(|id| run(&["--repo", host.path(), "show", id], &[]).exit() == 0)
            .count();
        let tracking = ref_in(host.path(), TRACKING_MAIN);
        objects_only |= !stored.is_empty() && adopted == 0;
        tracking_behind |= adopted == changesets.len() && tracking.as_ref() != Some(&head);

        // Resumed: nothing stored is asked for again, and it ends where an uncut fetch does.
        let log = host.tmp().join(format!("answers-{k}"));
        let resumed = site.fetch(host.path(), &site.ssh_logged(&log));
        assert_eq!(resumed.exit(), 0, "resumed after a crash at {k}: {resumed}");
        let again: Vec<ObjectId> = received(&log);
        let twice: Vec<&ObjectId> = again.iter().filter(|id| stored.contains(id)).collect();
        assert!(twice.is_empty(), "crash at {k}: asked again for {twice:?}");
        assert_eq!(
            ref_in(host.path(), TRACKING_MAIN).as_ref(),
            Some(&head),
            "crash at {k}"
        );
        assert_clean(host.path());
    }
    assert!(
        objects_only,
        "no crash left objects stored and none adopted (H1)"
    );
    assert!(
        tracking_behind,
        "no crash left every changeset adopted, the ref behind (H2)"
    );
}

/// An answer whose object bytes miss their id is refused, and no ref moves; the same fetch
/// through a sound connection then brings it.
#[test]
fn a_bad_object_from_the_remote_moves_no_ref() {
    let site = Site::new();
    let (host, _) = site.cloned();
    let old = site.main_head();
    let new = site.land("marked.txt", &"Q".repeat(64));
    let refs_before = run(&["--repo", host.path(), "ref", "list"], &[]).ok();

    // 1. Every Q the upstream answers turned into an R on the way, each read passed on at once.
    let damaging = site.ssh_through("stdbuf -o0 tr Q R");
    let refused = site.fetch(host.path(), &damaging);
    refused.err("E_INVALID_ARG");
    assert_eq!(
        run(&["--repo", host.path(), "ref", "list"], &[]).ok(),
        refs_before
    );
    assert_eq!(ref_in(host.path(), TRACKING_MAIN), Some(old));
    assert_clean(host.path());

    // 2. A sound connection.
    site.fetch(host.path(), &site.ssh()).ok();
    assert_eq!(ref_in(host.path(), TRACKING_MAIN), Some(new));
    assert_clean(host.path());
}

/// An upstream tag becomes the host tag of the same name once; the next fetch makes none.
#[test]
fn a_remote_tag_becomes_a_local_tag_once() {
    let site = Site::new();
    let head = site.main_head();
    site.so(&["ref", "set", "tags/v1", &head, "--create"]);

    let (host, cloned) = site.cloned();
    assert_eq!(cloned["fetch"]["tags_fetched"], 1, "{cloned}");
    assert_eq!(ref_in(host.path(), "tags/v1"), Some(head));

    let again = site.fetch(host.path(), &site.ssh()).ok();
    assert_eq!(again["tags_fetched"], 0, "{again}");
    assert!(again.get("warnings").is_none(), "{again}");
}

/// A host tag that names another changeset than the upstream's of the same name is kept, and
/// the fetch says so.
#[test]
fn a_differing_local_tag_is_kept_and_reported() {
    let site = Site::new();
    let (host, _) = site.cloned();
    let host_head = ref_in(host.path(), "main").expect("main");
    run(
        &[
            "--repo",
            host.path(),
            "--as",
            AGENT,
            "ref",
            "set",
            "tags/v2",
            &host_head,
            "--create",
        ],
        &[],
    )
    .ok();
    let newer = site.land("c.txt", "c\n");
    site.so(&["ref", "set", "tags/v2", &newer, "--create"]);

    let fetched = site.fetch(host.path(), &site.ssh()).ok();
    assert_eq!(
        fetched["warnings"],
        serde_json::json!(["tag_differs: tags/v2"]),
        "{fetched}"
    );
    assert_eq!(fetched["tags_fetched"], 0, "{fetched}");
    assert_eq!(ref_in(host.path(), "tags/v2"), Some(host_head));
}

/// An upstream whose `main` is not ahead of the tracking ref moves no ref: the tracking ref of
/// `main` moves forward only.
#[test]
fn a_tracking_ref_of_main_never_moves_back() {
    let site = Site::new();
    let head = site.main_head();
    site.so(&["ref", "set", "feature/b", &head, "--create"]);
    let (host, _) = site.cloned();
    let refs_before = run(&["--repo", host.path(), "ref", "list"], &[]).ok();

    // 1. The served store replaced by one of another history, with a branch moved too.
    let store = site.served.store(STORE);
    fs::rename(&store, site.work.path().join("old-acme")).expect("set the store aside");
    site.served.init(STORE);
    site.grant();
    let other = site.land("other.txt", "other\n");
    site.so(&["ref", "set", "feature/b", &other, "--create"]);

    // 2. Refused, nothing moved.
    let refused = site.fetch(host.path(), &site.ssh());
    let error = refused.err("E_REF_PROTECTED");
    assert_eq!(error["details"]["name"], TRACKING_MAIN, "{error}");
    assert_eq!(
        run(&["--repo", host.path(), "ref", "list"], &[]).ok(),
        refs_before
    );
    assert_clean(host.path());
}

/// A host store of FORMAT 1 is refused before ssh is started.
#[test]
fn a_format_1_store_is_refused_before_any_byte() {
    let site = Site::new();
    let host = Store::init_as(swarmops_store::StoreFormat::V1);
    run(&["--repo", host.path(), "remote", "add", ORIGIN, URL], &[]).ok();
    let marker = site.work.path().join("ssh-started");
    let script = site.work.path().join("ssh-marker");
    write_script(
        &script,
        &format!("#!/bin/sh\ntouch '{}'\n", path_text(&marker)),
    );

    let refused = site.fetch(host.path(), &path_text(&script));
    let error = refused.err("E_FORMAT_REQUIRED");
    assert_eq!(error["details"]["needs"], 2, "{error}");
    assert!(!marker.exists(), "ssh was started");
}

/// A `so://` URL with a password is refused by `remote add` and by `clone`, which makes no
/// store.
#[test]
fn a_so_url_with_a_password_is_refused() {
    let site = Site::new();
    let url = "so://ada:secret@hub.example/acme";
    let host = Store::init();
    let refused = run(&["--repo", host.path(), "remote", "add", ORIGIN, url], &[]);
    let error = refused.err("E_INVALID_ARG");
    assert_eq!(error["details"]["reason"], "credential_in_url", "{error}");

    let into = site.work.path().join("clone");
    let into = path_text(&into);
    let refused = run(&["--as", AGENT, "clone", url, "--into", &into], &[]);
    let error = refused.err("E_INVALID_ARG");
    assert_eq!(error["details"]["reason"], "credential_in_url", "{error}");
    assert!(!Path::new(&into).exists());
    let envelope = refused.envelope().to_string();
    assert!(!envelope.contains("secret"), "{envelope}");
}

/// A server older than native sync answers with git's `ERR` line: the host says it is not
/// native and names the git wire's address, with no fallback.
#[test]
fn an_old_server_is_not_native_with_the_git_url_hint() {
    let site = Site::new();
    let host = Store::init();
    run(&["--repo", host.path(), "remote", "add", ORIGIN, URL], &[]).ok();
    let script = site.work.path().join("ssh-old");
    let pkt = format!("{:04x}{OLD_SERVER_SAYS}", OLD_SERVER_SAYS.len() + 4);
    write_script(
        &script,
        &format!("#!/bin/sh\nprintf '%s' '{pkt}'\nexit 2\n"),
    );

    let refused = site.fetch(host.path(), &path_text(&script));
    let error = refused.err("E_REMOTE_REJECTED");
    assert_eq!(error["details"]["reason"], "not_native", "{error}");
    let hint = error["hint"].as_str().expect("a hint");
    assert!(hint.contains("ssh://"), "{hint}");
    assert_eq!(ref_in(host.path(), TRACKING_MAIN), None);
}

/// A history of merges (an octopus and a criss-cross among them) and a rename crosses with
/// every id and every parent.
#[test]
fn merges_and_renames_cross_with_their_ids() {
    if let Some(why) = git::skipped("merges") {
        eprintln!("skipped: {why}");
        return;
    }
    let site = Site::bare();
    let merges = fixture("merges");
    let source = path_text(merges.repo.as_deref().expect("the fixture's repository"));
    site.so(&["git", "import", &source, "--ref", "HEAD", "--into", "main"]);
    let renamed = serde_json::json!([
        {"path": "base.txt", "op": "delete"},
        {"path": "moved/base.txt", "op": "put", "content": {"inline": "base\nfeature\n"}},
    ]);
    site.advance("main", &renamed);

    let (host, _) = site.cloned();
    let crossed = history(host.path(), "main");
    assert_eq!(crossed, history(&site.store(), "main"));
    assert!(
        crossed.iter().any(|(_, parents)| parents.len() > 2),
        "no octopus crossed"
    );
    assert_clean(host.path());
}

// ---- sync and push (the native sync design §1 N3–N10, §2.4 S1–S4, §6 N-5) ----

/// The served store's grants for hosts that write: ada reads, pushes any ref and lands on
/// `main`; bob only reads.
const WRITE_ACCESS: &str = r#"version = 1

[[grant]]
who = ["person/ada"]
read = true
push = ["**"]
land = ["main"]

[[grant]]
who = ["person/bob"]
read = true
"#;

/// The identity of a key that may only read.
const BOB: &str = "person/bob";

/// The agent of a second host.
const OTHER_AGENT: &str = "agent/worker-18";

impl Site {
    /// [`Site::new`], ada granted read, push and land, bob read ([`WRITE_ACCESS`]).
    fn writable() -> Site {
        let site = Site::new();
        site.grant_writes();
        site
    }

    /// Writes [`WRITE_ACCESS`] as the served store's `access.toml`.
    fn grant_writes(&self) {
        let access = self.served.store(STORE).join("access.toml");
        fs::write(access, WRITE_ACCESS).expect("write access");
    }

    /// `sync origin` on `host` through `ssh`, with `args` after it.
    fn sync(&self, host: &str, ssh: &str, args: &[&str]) -> Run {
        let mut line = vec!["sync", ORIGIN];
        line.extend(args);
        self.on(host, ssh, &line, &[])
    }

    /// `push origin <target>` on `host` through `ssh`.
    fn push(&self, host: &str, ssh: &str, target: &str) -> Run {
        self.on(host, ssh, &["push", ORIGIN, target], &[])
    }
}

/// A change on the host's `main` putting `content` at `path` as [`AGENT`], `main` moved to it.
fn host_commit(host: &str, path: &str, content: &str) -> String {
    commit_as(host, AGENT, path, content)
}

/// A change on `main` of the store at `store` putting `content` at `path` as `agent`, `main`
/// moved to it.
fn commit_as(store: &str, agent: &str, path: &str, content: &str) -> String {
    let changes = serde_json::json!([{"path": path, "op": "put", "content": {"inline": content}}]);
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("changes.json");
    fs::write(&file, changes.to_string()).expect("write the changes");
    let line = [
        "--repo",
        store,
        "--as",
        agent,
        "commit",
        "-m",
        path,
        "--changes",
        &path_text(&file),
        "--advance",
        "main",
    ];
    text(&run(&line, &[]).ok()["changeset"])
}

/// `show <id>`'s result in the store at `store`.
fn show(store: &str, id: &str) -> Value {
    run(&["--repo", store, "show", id], &[]).ok()
}

/// Each changeset of `main`'s first-parent chain in the store at `store`, newest first.
fn first_parents(store: &str) -> Vec<String> {
    let mut chain = Vec::new();
    let mut at = ref_in(store, "main");
    while let Some(id) = at {
        let shown = show(store, &id);
        at = shown["parents"]
            .as_array()
            .and_then(|p| p.first())
            .map(text);
        chain.push(id);
    }
    chain
}

/// How many times each of `changes` was landed on `main` of the store at `store`: how many of
/// its first parents name it in `squashed_from`.
fn landed_counts(store: &str, changes: &[String]) -> Vec<usize> {
    let mut counts = vec![0; changes.len()];
    for id in first_parents(store) {
        let shown = show(store, &id);
        let from = shown["squashed_from"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for (n, change) in changes.iter().enumerate() {
            counts[n] += from
                .iter()
                .filter(|named| *named == change.as_str())
                .count();
        }
    }
    counts
}

/// Asserts that each of `changes` is landed on the served `main` exactly once.
fn assert_landed_once(site: &Site, changes: &[String]) {
    let counts = landed_counts(&site.store(), changes);
    assert_eq!(counts, vec![1; changes.len()], "{changes:?}");
}

/// A sync lands each change of the host on the upstream, adopts what landed with its ids, and
/// moves the host's `main` to the upstream's head: both hold the same history.
#[test]
fn sync_moves_main_to_the_upstream_head_with_the_same_ids() {
    let site = Site::writable();
    let (host, _) = site.cloned();
    let old = ref_in(host.path(), "main").expect("main");
    let changes = [
        host_commit(host.path(), "a.txt", "a\n"),
        host_commit(host.path(), "b.txt", "b\n"),
    ];
    let mine = ref_in(host.path(), "main").expect("main");

    let synced = site.sync(host.path(), &site.ssh(), &[]).ok();
    let head = site.main_head();
    assert_eq!(synced["status"], "pushed", "{synced}");
    assert_eq!(synced["main"]["old"], mine.as_str(), "{synced}");
    assert_eq!(synced["main"]["new"], head.as_str(), "{synced}");
    let published = synced["published"].as_array().expect("published");
    assert_eq!(published.len(), 2, "{synced}");
    for (entry, change) in published.iter().zip(&changes) {
        assert_eq!(entry["changeset"], change.as_str(), "{synced}");
    }
    assert_eq!(published[1]["landed_as"], head.as_str(), "{synced}");

    // The same ids, parents and all, on both; the changes once each, above the old head.
    assert_eq!(ref_in(host.path(), "main"), Some(head.clone()));
    assert_eq!(ref_in(host.path(), TRACKING_MAIN), Some(head));
    assert_eq!(history(host.path(), "main"), history(&site.store(), "main"));
    assert_eq!(first_parents(&site.store())[2], old);
    assert_landed_once(&site, &changes);
    assert_clean(host.path());
    assert_clean(&site.store());
}

/// Each change is landed by the upstream once, under the key of its id on `main`; a sync
/// again lands nothing.
#[test]
fn push_lands_each_change_once_with_its_key() {
    let site = Site::writable();
    let (host, _) = site.cloned();
    let changes = [
        host_commit(host.path(), "a.txt", "a\n"),
        host_commit(host.path(), "b.txt", "b\n"),
        host_commit(host.path(), "c.txt", "c\n"),
    ];
    site.sync(host.path(), &site.ssh(), &[]).ok();

    // 1. One landed changeset per change, each keyed ss1-, the keys apart.
    let landed: Vec<Value> = first_parents(&site.store())[..3]
        .iter()
        .map(|id| show(&site.store(), id))
        .collect();
    let mut keys = HashSet::new();
    for shown in &landed {
        let key = text(&shown["land_key"]);
        assert!(key.starts_with("ss1-") && key.len() == 64, "{shown}");
        assert!(keys.insert(key), "{shown}");
    }
    assert_landed_once(&site, &changes);

    // 2. Again: up to date, nothing landed, no frame on the upstream.
    let journal = run(&["--repo", &site.store(), "op", "log"], &[]).ok();
    let again = site.sync(host.path(), &site.ssh(), &[]).ok();
    assert_eq!(again["status"], "up_to_date", "{again}");
    assert_eq!(again["published"], serde_json::json!([]), "{again}");
    assert_eq!(
        run(&["--repo", &site.store(), "op", "log"], &[]).ok(),
        journal
    );
}

/// A land whose answer never reached the host (U5) is not landed again: the next sync finds
/// it by its key and moves `main` to the upstream's head.
#[test]
fn a_push_whose_answer_was_lost_replays() {
    let site = Site::writable();
    let (host, _) = site.cloned();
    let change = host_commit(host.path(), "a.txt", "a\n");
    let mine = ref_in(host.path(), "main").expect("main");

    // 1. The upstream lands; its answer is dropped on the way, and the connection with it.
    let losing = site.ssh_through("{ LC_ALL=C sed -u '/\"op\":\"wire_land\"/Q'; kill -KILL $$; }");
    let cut = site.sync(host.path(), &losing, &[]);
    let error = cut.err("E_REMOTE_UNREACHABLE");
    assert_eq!(error["retryable"], true, "{cut}");
    assert_landed_once(&site, std::slice::from_ref(&change));
    assert_eq!(ref_in(host.path(), "main"), Some(mine));

    // 2. Again: nothing lands twice; main is the upstream's head.
    let again = site.sync(host.path(), &site.ssh(), &[]).ok();
    assert_eq!(again["published"], serde_json::json!([]), "{again}");
    let dropped = again["dropped"].as_array().expect("dropped");
    assert_eq!(dropped.len(), 1, "{again}");
    assert_eq!(dropped[0]["changeset"], change.as_str(), "{again}");
    assert_eq!(ref_in(host.path(), "main"), Some(site.main_head()));
    assert_landed_once(&site, &[change]);
    assert_clean(host.path());
}

/// A change another host landed already, the same changeset, is recognised by its key and not
/// landed again.
#[test]
fn a_change_landed_from_another_host_is_not_landed_twice() {
    let site = Site::writable();
    let (first, _) = site.cloned();
    let change = host_commit(first.path(), "a.txt", "a\n");
    let second = Store::copy_of(&first);

    site.sync(first.path(), &site.ssh(), &[]).ok();
    let head = site.main_head();
    let again = site.sync(second.path(), &site.ssh(), &[]).ok();
    assert_eq!(again["published"], serde_json::json!([]), "{again}");
    assert_eq!(again["dropped"][0]["changeset"], change.as_str(), "{again}");
    assert_eq!(site.main_head(), head);
    assert_eq!(ref_in(second.path(), "main"), Some(head));
    assert_landed_once(&site, &[change]);
}

/// Two hosts that sync at once each land every change of theirs once; both end on the same
/// upstream head, and the upstream's `main` stays linear.
#[test]
fn two_hosts_sync_at_once_and_each_change_lands_once() {
    let site = Site::writable();
    let (first, _) = site.cloned();
    let second = Store::copy_of(&first);
    let mut changes = Vec::new();
    for n in 0..3 {
        changes.push(host_commit(first.path(), &format!("first-{n}.txt"), "1\n"));
        changes.push(commit_as(
            second.path(),
            OTHER_AGENT,
            &format!("second-{n}.txt"),
            "2\n",
        ));
    }

    // 1. Both at once, each until it is done (a lost race is retryable).
    let ssh = site.ssh();
    std::thread::scope(|scope| {
        for host in [&first, &second] {
            let ssh = &ssh;
            let site = &site;
            scope.spawn(move || {
                for _ in 0..8 {
                    let synced = site.sync(host.path(), ssh, &[]);
                    if synced.exit() == 0 {
                        return;
                    }
                    assert_eq!(synced.envelope()["error"]["retryable"], true, "{synced}");
                }
                panic!("no sync of {} succeeded", host.path());
            });
        }
    });

    // 2. Each lands once; both hosts end on the upstream's head after one more sync.
    assert_landed_once(&site, &changes);
    for host in [&first, &second] {
        site.sync(host.path(), &site.ssh(), &[]).ok();
        assert_eq!(ref_in(host.path(), "main"), Some(site.main_head()));
        assert_clean(host.path());
    }
    let linear = history(&site.store(), "main");
    assert!(
        linear.iter().all(|(_, parents)| parents.len() <= 1),
        "{linear:?}"
    );
    assert_clean(&site.store());
}

/// A change that conflicts with the upstream's `main` is `E_CONFLICT`, naming it, its agent and
/// the paths; it and the later ones are not landed, the earlier ones are, and the host's `main`
/// ends at the upstream's head, before the refused ones, never past them.
#[test]
fn a_conflict_names_the_change_and_its_paths_and_moves_no_main() {
    let site = Site::writable();
    let (host, _) = site.cloned();
    let early = host_commit(host.path(), "early.txt", "early\n");
    let clash = host_commit(host.path(), "README.md", "host\n");
    let late = host_commit(host.path(), "late.txt", "late\n");
    let theirs = site.land("README.md", "upstream\n");

    let failed = site.sync(host.path(), &site.ssh(), &[]);
    let error = failed.err("E_CONFLICT");
    assert_eq!(error["details"]["change"], clash.as_str(), "{failed}");
    assert_eq!(error["details"]["agent"], AGENT, "{failed}");
    let refused = serde_json::json!([clash, late]);
    assert_eq!(error["details"]["refused"], refused, "{failed}");
    assert!(failed.to_string().contains("README.md"), "{failed}");

    // The early change landed once, the clash and the late one not; main is the upstream's.
    assert_landed_once(&site, std::slice::from_ref(&early));
    assert_eq!(landed_counts(&site.store(), &[clash, late]), [0, 0]);
    let head = site.main_head();
    assert_ne!(head, theirs);
    assert_eq!(ref_in(host.path(), "main"), Some(head));
    assert_clean(host.path());
}

/// The refused change, rebased on the host onto what it was refused against and resolved
/// there, is landed by the next sync under its new id; the refused id never is.
#[test]
fn a_conflict_resolved_on_the_host_lands_at_the_next_sync() {
    let site = Site::writable();
    let (host, _) = site.cloned();
    let base = ref_in(host.path(), "main").expect("main");
    let clash = host_commit(host.path(), "README.md", "host\n");
    let theirs = site.land("README.md", "upstream\n");
    site.sync(host.path(), &site.ssh(), &[]).err("E_CONFLICT");
    assert_eq!(ref_in(host.path(), "main"), Some(theirs.clone()));

    // 1. The refused change rebased onto main, which is the upstream's, its side taken.
    let resolved = site.work.path().join("resolved.json");
    let resolutions = serde_json::json!({
        "against": theirs,
        "files": {"README.md": {"take": "ours"}},
    });
    fs::write(&resolved, resolutions.to_string()).expect("write the resolutions");
    let rebased = run(
        &[
            "--repo",
            host.path(),
            "--as",
            AGENT,
            "rebase",
            &clash,
            "--base",
            &base,
            "--resolve",
            &path_text(&resolved),
        ],
        &[],
    )
    .ok();
    let again = text(&rebased["changeset"]);
    let set = ["ref", "set", "main", &again, "--expect", &theirs];
    let mut line = vec!["--repo", host.path(), "--as", AGENT];
    line.extend(set);
    run(&line, &[]).ok();

    // 2. The next sync lands it; the refused id never lands.
    let synced = site.sync(host.path(), &site.ssh(), &[]).ok();
    assert_eq!(
        synced["published"][0]["changeset"],
        again.as_str(),
        "{synced}"
    );
    assert_landed_once(&site, &[again]);
    assert_eq!(landed_counts(&site.store(), &[clash]), [0]);
    let readme = run(&["--repo", &site.store(), "cat", "main:README.md"], &[]).ok();
    assert_eq!(readme["content"], "host\n", "{readme}");
    assert_eq!(ref_in(host.path(), "main"), Some(site.main_head()));
}

/// Work done with no network syncs later: every change lands once, in order.
#[test]
fn offline_work_syncs_later_in_order() {
    let site = Site::writable();
    let (host, _) = site.cloned();
    let offline = site.offline();
    site.sync(host.path(), &offline, &[])
        .err("E_REMOTE_UNREACHABLE");
    let changes: Vec<String> = (0..4)
        .map(|n| host_commit(host.path(), &format!("offline-{n}.txt"), "o\n"))
        .collect();
    site.sync(host.path(), &offline, &[])
        .err("E_REMOTE_UNREACHABLE");

    site.sync(host.path(), &site.ssh(), &[]).ok();
    assert_landed_once(&site, &changes);
    let landed: Vec<String> = first_parents(&site.store())[..4]
        .iter()
        .rev()
        .map(|id| text(&show(&site.store(), id)["squashed_from"][0]))
        .collect();
    assert_eq!(landed, changes);
}

/// With no network, everything on the host's own store runs; fetch, push and sync fail
/// retryable, and nothing moves.
#[test]
fn a_host_with_no_network_commits_lands_reads_and_verifies() {
    let site = Site::writable();
    let (host, _) = site.cloned();
    let offline = site.offline();

    // 1. The host's own operations.
    let change = host_commit(host.path(), "a.txt", "a\n");
    let staged = commit_on(host.path(), &change, "b.txt");
    let landed = run(
        &["--repo", host.path(), "--as", AGENT, "land", &staged],
        &[],
    )
    .ok();
    assert_eq!(landed["status"], "landed", "{landed}");
    run(&["--repo", host.path(), "log", "main"], &[]).ok();
    assert_clean(host.path());

    // 2. The remote's: refused retryable, no ref moved.
    let branch = [
        "--repo",
        host.path(),
        "--as",
        AGENT,
        "ref",
        "set",
        "feature/o",
        &change,
    ];
    run(&[&branch[..], &["--create"]].concat(), &[]).ok();
    let refs = run(&["--repo", host.path(), "ref", "list"], &[]).ok();
    for args in [
        vec!["fetch", ORIGIN],
        vec!["push", ORIGIN, "feature/o"],
        vec!["sync", ORIGIN],
    ] {
        let failed = site.on(host.path(), &offline, &args, &[]);
        let error = failed.err("E_REMOTE_UNREACHABLE");
        assert_eq!(error["retryable"], true, "{failed}");
    }
    assert_eq!(run(&["--repo", host.path(), "ref", "list"], &[]).ok(), refs);
}

/// A changeset on `parent` in the store at `store` putting `path`, which no ref names.
fn commit_on(store: &str, parent: &str, path: &str) -> String {
    let changes = serde_json::json!([{"path": path, "op": "put", "content": {"inline": "x\n"}}]);
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("changes.json");
    fs::write(&file, changes.to_string()).expect("write the changes");
    let line = [
        "--repo",
        store,
        "--as",
        AGENT,
        "commit",
        "-m",
        path,
        "--changes",
        &path_text(&file),
        "--parent",
        parent,
    ];
    text(&run(&line, &[]).ok()["changeset"])
}

impl Site {
    /// An ssh command that reaches no host, as ssh with no route does: exit 255, nothing said.
    fn offline(&self) -> String {
        let script = self.work.path().join("ssh-offline");
        write_script(&script, "#!/bin/sh\nexit 255\n");
        path_text(&script)
    }
}

/// The upstream's op log names the connection's identity as the lander of each change, and
/// the landed changeset keeps the change's agent as its author.
#[test]
fn the_remote_log_names_the_lander_and_the_change_keeps_its_agent() {
    let site = Site::writable();
    let (host, _) = site.cloned();
    host_commit(host.path(), "a.txt", "a\n");
    site.sync(host.path(), &site.ssh(), &[]).ok();

    let landed = show(&site.store(), &site.main_head());
    assert_eq!(landed["author"]["agent"], AGENT, "{landed}");
    assert_eq!(landed["lander"]["agent"], ADA, "{landed}");
    let log = run(&["--repo", &site.store(), "op", "log"], &[]).ok();
    let lands: Vec<&Value> = (log["ops"].as_array().expect("ops").iter())
        .filter(|op| op["kind"] == "land")
        .collect();
    assert_eq!(lands.len(), 1, "{log}");
    assert_eq!(lands[0]["ref_moves"][0]["actor"], ADA, "{log}");
    assert_eq!(lands[0]["changesets"][0]["agent"], AGENT, "{log}");
    assert!(!log.to_string().contains("unknown/unknown"), "{log}");
}

/// A landed changeset names the change it came from in `squashed_from` and its key in
/// `land_key`, in both stores.
#[test]
fn landed_changes_name_their_origin() {
    let site = Site::writable();
    let (host, _) = site.cloned();
    let change = host_commit(host.path(), "a.txt", "a\n");
    site.sync(host.path(), &site.ssh(), &[]).ok();
    let head = site.main_head();

    for store in [site.store(), host.path().to_owned()] {
        let shown = show(&store, &head);
        assert_eq!(
            shown["squashed_from"],
            serde_json::json!([change]),
            "{shown}"
        );
        assert!(text(&shown["land_key"]).starts_with("ss1-"), "{shown}");
    }
}

/// Each agent's key is decided by its own grants: ada's lands, bob's, which only reads, is
/// refused and moves nothing.
#[test]
fn one_key_per_agent_is_decided_by_its_own_grants() {
    let site = Site::writable();
    let (ada, _) = site.cloned();
    let bob = Store::copy_of(&ada);
    let mine = host_commit(ada.path(), "ada.txt", "a\n");
    let his = commit_as(bob.path(), OTHER_AGENT, "bob.txt", "b\n");
    let bob_main = ref_in(bob.path(), "main");

    site.sync(ada.path(), &site.ssh(), &[]).ok();
    let refused = site.sync(bob.path(), &site.served.ssh_command(BOB), &[]);
    assert_eq!(refused.envelope()["ok"], false, "{refused}");
    assert_landed_once(&site, &[mine]);
    assert_eq!(landed_counts(&site.store(), &[his]), [0]);
    assert_eq!(ref_in(bob.path(), "main"), bob_main);
}

/// The upstream's refusals reach the host as its envelopes: a caller with no grant, a ref rule,
/// a conflict.
#[test]
fn refusals_reach_the_client_as_envelopes() {
    let site = Site::writable();
    let (host, _) = site.cloned();
    host_commit(host.path(), "a.txt", "a\n");

    // 1. No grant to upload.
    let refused = site.sync(host.path(), &site.served.ssh_command(BOB), &[]);
    let error = refused.envelope()["error"].clone();
    assert_eq!(error["code"], "E_POLICY_DENIED", "{refused}");

    // 2. A tag the upstream holds at another changeset is never moved.
    let head = site.main_head();
    site.so(&["ref", "set", "tags/v1", &head, "--create"]);
    let mine = ref_in(host.path(), "main").expect("main");
    let tag = [
        "--repo",
        host.path(),
        "--as",
        AGENT,
        "ref",
        "set",
        "tags/v1",
        &mine,
        "--create",
    ];
    run(&tag, &[]).ok();
    site.push(host.path(), &site.ssh(), "tags/v1")
        .err("E_TAG_IMMUTABLE");
    assert_eq!(site.ref_at("tags/v1"), Some(head));

    // 3. A conflict.
    host_commit(host.path(), "README.md", "host\n");
    site.land("README.md", "upstream\n");
    site.sync(host.path(), &site.ssh(), &[]).err("E_CONFLICT");
}

/// The upstream's store held busy by another writer for the whole of its lock's wait reaches
/// the host as `E_BUSY`, retryable, and nothing moves.
#[test]
#[ignore = "slow, ~12 s: waits out the upstream store's 10 s lock timeout"]
fn a_busy_upstream_reaches_the_client_as_e_busy() {
    use std::time::Duration;

    use swarmops_store::MetaLock;

    let site = Site::writable();
    let (host, _) = site.cloned();
    host_commit(host.path(), "a.txt", "a\n");
    let main = ref_in(host.path(), "main");

    let layout = swarmops_testkit::store::layout_of(site.store());
    let held = MetaLock::acquire(&layout, Duration::from_secs(1)).expect("take meta/lock");
    let busy = site.sync(host.path(), &site.ssh(), &["--max-attempts", "1"]);
    drop(held);
    let error = busy.err("E_BUSY");
    assert_eq!(error["retryable"], true, "{busy}");
    assert_eq!(ref_in(host.path(), "main"), main);
}

/// A branch pushed is created, then advanced, on the upstream at the host's own ids.
#[test]
fn push_of_a_branch_advances_it_with_ids_kept() {
    let site = Site::writable();
    let (host, _) = site.cloned();
    let main = ref_in(host.path(), "main").expect("main");
    let first = commit_on(host.path(), &main, "x.txt");
    let set = |new: &str, old: Option<&str>| {
        let mut line = vec![
            "--repo",
            host.path(),
            "--as",
            AGENT,
            "ref",
            "set",
            "feature/x",
            new,
        ];
        match old {
            Some(old) => line.extend(["--expect", old]),
            None => line.push("--create"),
        }
        run(&line, &[]).ok();
    };
    set(&first, None);

    let created = site.push(host.path(), &site.ssh(), "feature/x").ok();
    assert_eq!(created["status"], "created", "{created}");
    assert_eq!(created["head_changeset"], first.as_str(), "{created}");
    assert_eq!(site.ref_at("feature/x"), Some(first.clone()));

    let second = commit_on(host.path(), &first, "y.txt");
    set(&second, Some(&first));
    let pushed = site.push(host.path(), &site.ssh(), "feature/x").ok();
    assert_eq!(pushed["status"], "pushed", "{pushed}");
    assert_eq!(site.ref_at("feature/x"), Some(second.clone()));
    assert_eq!(
        history(&site.store(), "feature/x"),
        history(host.path(), "feature/x")
    );
    assert_eq!(ref_in(host.path(), "remote/origin/feature/x"), Some(second));
    let again = site.push(host.path(), &site.ssh(), "feature/x").ok();
    assert_eq!(again["status"], "up_to_date", "{again}");
    assert_clean(&site.store());
}

/// A tag pushed is created at the host's id; one the upstream holds elsewhere is never moved.
#[test]
fn a_tag_is_created_and_never_moved() {
    let site = Site::writable();
    let (host, _) = site.cloned();
    let main = ref_in(host.path(), "main").expect("main");
    let tagged = commit_on(host.path(), &main, "t.txt");
    for (tag, at) in [("tags/v1", &tagged), ("tags/v2", &tagged)] {
        let line = [
            "--repo",
            host.path(),
            "--as",
            AGENT,
            "ref",
            "set",
            tag,
            at,
            "--create",
        ];
        run(&line, &[]).ok();
    }
    site.so(&["ref", "set", "tags/v2", &main, "--create"]);

    let created = site.push(host.path(), &site.ssh(), "tags/v1").ok();
    assert_eq!(created["status"], "created", "{created}");
    assert_eq!(site.ref_at("tags/v1"), Some(tagged));
    let again = site.push(host.path(), &site.ssh(), "tags/v1").ok();
    assert_eq!(again["status"], "up_to_date", "{again}");

    site.push(host.path(), &site.ssh(), "tags/v2")
        .err("E_TAG_IMMUTABLE");
    assert_eq!(site.ref_at("tags/v2"), Some(main));
}

/// A first sync to an upstream with no `main` creates it at the host's head, ids kept.
#[test]
fn the_first_push_to_an_empty_remote_keeps_ids() {
    let site = Site::bare();
    site.grant_writes();
    let host = Store::init();
    run(&["--repo", host.path(), "remote", "add", ORIGIN, URL], &[]).ok();
    host_commit(host.path(), "a.txt", "a\n");
    let head = host_commit(host.path(), "b.txt", "b\n");

    let synced = site.sync(host.path(), &site.ssh(), &[]).ok();
    assert_eq!(synced["status"], "pushed", "{synced}");
    assert_eq!(site.ref_at("main"), Some(head.clone()));
    assert_eq!(history(&site.store(), "main"), history(host.path(), "main"));
    assert_eq!(ref_in(host.path(), TRACKING_MAIN), Some(head));
    assert_clean(&site.store());
}

/// `main` and `release/*` are landed, never moved, by a native push: until a push lands them,
/// it says to sync.
#[test]
fn a_push_of_main_says_to_sync() {
    let site = Site::writable();
    let (host, _) = site.cloned();
    host_commit(host.path(), "a.txt", "a\n");
    let refused = site.push(host.path(), &site.ssh(), "main");
    let error = refused.err("E_INVALID_ARG");
    assert_eq!(error["details"]["reason"], "land_with_sync", "{refused}");
    assert_eq!(site.ref_at("main"), Some(site.main_head()));
}

/// `sync status` of a native remote reads its tracking ref, with no network.
#[test]
fn sync_status_reads_the_tracking_ref() {
    let site = Site::writable();
    let (host, _) = site.cloned();
    let head = site.main_head();
    host_commit(host.path(), "a.txt", "a\n");
    let status = site
        .on(
            host.path(),
            &site.offline(),
            &["sync", "status", ORIGIN],
            &[],
        )
        .ok();
    assert_eq!(status["fetched"], true, "{status}");
    assert_eq!(
        status["remote_head"]["changeset"],
        head.as_str(),
        "{status}"
    );
    assert_eq!(status["ahead"], 1, "{status}");
    assert_eq!(status["behind"], 0, "{status}");
}
