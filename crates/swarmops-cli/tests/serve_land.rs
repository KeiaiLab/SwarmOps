//! Pushes to `refs/land/<target>` through `swarmops serve` (M14 design §7), as stock git makes
//! them over ssh through the test kit's forced-command stand-in: the hook imports the pushed tip
//! and lands it onto the target with a key of who pushed which tip where, so that the same push
//! again replays; the client's tracking branch moves to the landed commit, and a conflict is
//! reported on `remote:` lines with nothing written.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::thread;

use serde_json::Value;
use swarmops_git::oid::GitOid;
use swarmops_git::serve::land_key::land_key;
use swarmops_testkit::git::fixture;
use swarmops_testkit::serve::ServeSite;
use swarmops_testkit::store::layout_of;
use tempfile::TempDir;

use crate::support::{self, Swarmops};

/// The agent a forced command names.
const AGENT: &str = "person/ada";

/// The agent that writes the served store directly, through the command line.
const WRITER: &str = "agent/worker-1";

/// The store every site serves.
const STORE: &str = "acme";

/// The served store's access file: every person pushes and lands onto `main`, and pushes the
/// release branches.
const ACCESS: &str = r#"version = 1

[[grant]]
who = ["person/*"]
push = ["main", "release/*"]
land = ["main"]
"#;

/// Where a push names the branch it lands onto.
const LAND_MAIN: &str = "HEAD:refs/land/main";

/// How many clients push to `refs/land/main` at once, and how many agents land beside them, in
/// criterion 4's case.
const PUSHERS: usize = 8;
const LANDERS: usize = 4;

/// A served root with the store [`STORE`] holding the `linear` fixture as `main`, under
/// [`ACCESS`], and a directory for clients.
struct Site {
    /// The root, its stand-ins and the client git.
    served: ServeSite,
    /// The clients' working directories.
    work: TempDir,
}

impl Site {
    /// The store, served with the test kit's environment.
    fn new() -> Site {
        let served = ServeSite::new::<Swarmops>();
        served.init(STORE);
        let site = Site {
            served,
            work: TempDir::new().unwrap(),
        };
        fs::write(site.served.store(STORE).join("access.toml"), ACCESS).unwrap();
        let linear = fixture("linear");
        let source = linear.repo.as_deref().unwrap().to_str().unwrap();
        site.so(&["git", "import", source, "--ref", "HEAD", "--into", "main"]);
        site
    }

    /// The store's path.
    fn store(&self) -> String {
        self.served.store(STORE).to_str().unwrap().to_owned()
    }

    /// `args` on the store as `agent`; what it did.
    fn so_as(&self, agent: &str, args: &[&str]) -> support::Run {
        let store = self.store();
        let line: Vec<&str> = ["--repo", &store, "--as", agent]
            .iter()
            .chain(args)
            .copied()
            .collect();
        support::run(&line, &[])
    }

    /// `args` on the store as [`WRITER`]; its result.
    fn so(&self, args: &[&str]) -> Value {
        self.so_as(WRITER, args).ok()
    }

    /// The length of the store's journal.
    fn journal_len(&self) -> u64 {
        let log = layout_of(self.store()).journal_log();
        fs::metadata(log).unwrap().len()
    }

    /// The commit `serve.git`'s `full` names, if any.
    fn served_ref(&self, full: &str) -> Option<String> {
        let output = self
            .served
            .command()
            .arg("--git-dir")
            .arg(self.served.store(STORE).join("git/serve.git"))
            .args(["for-each-ref", "--format=%(objectname) %(refname)"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", text(&output.stderr));
        let listed = text(&output.stdout);
        let found = listed.lines().find_map(|line| {
            let (oid, name) = line.split_once(' ')?;
            (name == full).then(|| oid.to_owned())
        });
        assert!(
            !listed.contains("refs/land/"),
            "a ref under refs/land/: {listed}"
        );
        found
    }

    /// git with `args` in `dir` as `who`; what it did. Its stderr never holds the store's path.
    fn git_as(&self, who: &str, dir: &Path, args: &[&str]) -> Output {
        let output = self
            .served
            .command()
            .env("GIT_SSH_COMMAND", self.served.ssh_command(who))
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        let stderr = text(&output.stderr);
        assert!(!stderr.contains(&self.store()), "a host path: {stderr}");
        output
    }

    /// git with `args` in `dir` as [`AGENT`], which must succeed; its stdout.
    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let output = self.git_as(AGENT, dir, args);
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            text(&output.stderr)
        );
        text(&output.stdout)
    }

    /// The push of `dir`'s `HEAD` to `refs/land/main` as [`AGENT`], which must succeed; its
    /// stderr.
    fn push_land(&self, dir: &Path) -> String {
        let pushed = self.git_as(AGENT, dir, &["push", "origin", LAND_MAIN]);
        let stderr = text(&pushed.stderr);
        assert!(pushed.status.success(), "{stderr}");
        stderr
    }

    /// The store cloned into `name`; the clone's directory.
    fn clone(&self, name: &str) -> PathBuf {
        let url = self.served.ssh_url(STORE);
        self.git(self.work.path(), &["clone", "--quiet", &url, name]);
        self.work.path().join(name)
    }

    /// A commit writing `content` to `path` in the clone `dir`, on its branch; its id.
    fn commit(&self, dir: &Path, path: &str, content: &str) -> String {
        fs::write(dir.join(path), content).unwrap();
        self.git(dir, &["add", path]);
        self.git(dir, &["commit", "--quiet", "-m", path]);
        self.rev(dir, "HEAD")
    }

    /// The clone's `rev-parse` of `rev` in `dir`.
    fn rev(&self, dir: &Path, rev: &str) -> String {
        self.git(dir, &["rev-parse", "--verify", rev])
            .trim()
            .to_owned()
    }

    /// The store's `main` head.
    fn main_head(&self) -> String {
        self.so(&["show", "main"])["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// The land keys of `main`'s first parents, newest first; `None` for a changeset no land
    /// made. Asks for more than there are, so that nothing is left out.
    fn land_keys(&self) -> Vec<Option<String>> {
        let log = self.so(&["log", "main", "-n", "100"]);
        assert_eq!(log["truncated"], false, "{log}");
        (log["entries"].as_array().unwrap().iter())
            .map(|entry| {
                let shown = self.so(&["show", entry["id"].as_str().unwrap()]);
                shown["land_key"].as_str().map(str::to_owned)
            })
            .collect()
    }

    /// Asserts that `verify --deep` finds nothing.
    fn assert_clean(&self) {
        let verify = support::run(&["--repo", &self.store(), "verify", "--deep"], &[]);
        assert_eq!(verify.exit(), 0, "{verify}");
    }
}

/// `bytes` as text.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The land key of `who` pushing the commit `tip` to `refs/land/main`.
fn key_of(who: &str, tip: &str) -> String {
    land_key(who, "main", GitOid::from_hex(tip.as_bytes()).unwrap())
}

/// A push to `refs/land/main` lands one squashed changeset keyed by the push, `serve.git`
/// serves it as `main` and keeps no `refs/land/` ref, and the client reads which commit landed,
/// which a fetch brings in; the same push again, after the client lost the move, replays: it
/// lands nothing, and the journal gains no frame.
#[test]
fn land_push_lands_and_replays() {
    let site = Site::new();
    let work = site.clone("work");
    let before = site.main_head();
    let tip = site.commit(&work, "landed.txt", "landed\n");

    // 1. The push: one changeset on main, squashed from the pushed commit, with the push's key.
    let stderr = site.push_land(&work);
    let shown = site.so(&["show", "main"]);
    assert_eq!(shown["parents"][0], before.as_str(), "{shown}");
    assert_eq!(shown["land_key"], key_of(AGENT, &tip).as_str(), "{shown}");
    assert_eq!(shown["lander"]["agent"], AGENT, "{shown}");

    // 2. serve.git serves the landed commit as main, and the client is told it; a fetch moves
    //    origin/main there.
    let served = site.served_ref("refs/heads/main").expect("main served");
    assert_ne!(served, tip);
    let told = format!("remote: landed onto main as {served}: git fetch brings it in");
    assert!(stderr.contains(&told), "{stderr}");
    site.git(&work, &["fetch", "--quiet", "origin"]);
    assert_eq!(site.rev(&work, "origin/main"), served);

    // 3. The same push again, its move lost: replayed, nothing written.
    let landed = site.main_head();
    site.git(&work, &["update-ref", "refs/remotes/origin/main", "HEAD~1"]);
    let journal = site.journal_len();
    let stderr = site.push_land(&work);
    assert_eq!(site.journal_len(), journal, "the replay wrote: {stderr}");
    assert_eq!(site.main_head(), landed);
    let told = format!("remote: landed already onto main as {served}");
    assert!(stderr.contains(&told), "{stderr}");
}

/// A push whose change conflicts with `main` is refused and lands nothing, and the client reads
/// the report on `remote:` lines: the count, the target's head as it fetches it, the path. The
/// push imports the pushed commits, so the journal is compared across the same push again: the
/// land itself writes nothing.
#[test]
fn land_push_conflict_writes_nothing() {
    let site = Site::new();
    let theirs = site.clone("theirs");
    let ours = site.clone("ours");

    // 1. main changes a file; the change to land changes the same line.
    let moved = site.commit(&theirs, "shared.txt", "theirs\n");
    site.git(&theirs, &["push", "--quiet", "origin", "main"]);
    site.commit(&ours, "shared.txt", "ours\n");
    let head = site.main_head();

    // 2. The push, twice: refused, main where it was, the second writing nothing at all.
    let refused = |site: &Site| {
        let pushed = site.git_as(AGENT, &ours, &["push", "origin", LAND_MAIN]);
        let stderr = text(&pushed.stderr);
        assert!(!pushed.status.success(), "{stderr}");
        assert!(stderr.contains("[remote rejected]"), "{stderr}");
        assert!(stderr.contains("(conflict)"), "{stderr}");
        stderr
    };
    refused(&site);
    let journal = site.journal_len();
    let stderr = refused(&site);
    assert_eq!(site.journal_len(), journal, "{stderr}");
    assert_eq!(site.main_head(), head);
    let keys = site.land_keys();
    assert!(keys.iter().all(Option::is_none), "{keys:?}");

    // 3. The report: the count and main's head as the client fetches it, then the path.
    let short = &moved[..7];
    let first = format!("remote: E_CONFLICT: 1 path conflicts with main at {short}");
    assert!(stderr.contains(&first), "{stderr}");
    assert!(
        stderr.contains("remote:   shared.txt: both changed lines 1;"),
        "{stderr}"
    );
    assert!(
        stderr.contains("remote: resolve it on top of main, then push again"),
        "{stderr}"
    );
}

/// A target the caller may not land onto, a tag as a target, and a tip `main` holds already:
/// each refused, nothing written.
#[test]
fn land_push_refusals() {
    let site = Site::new();
    let work = site.clone("work");
    site.so(&["ref", "set", "release/1", "main", "--create"]);
    let head = site.main_head();

    // 1. The tip main holds already: nothing to land.
    let nothing = site.git_as(AGENT, &work, &["push", "origin", LAND_MAIN]);
    let stderr = text(&nothing.stderr);
    assert!(!nothing.status.success(), "{stderr}");
    assert!(stderr.contains("E_NOTHING_TO_LAND"), "{stderr}");

    // 2. release/1: no land grant.
    site.commit(&work, "denied.txt", "denied\n");
    let line = ["push", "origin", "HEAD:refs/land/release/1"];
    let denied = site.git_as(AGENT, &work, &line);
    let stderr = text(&denied.stderr);
    assert!(!denied.status.success(), "{stderr}");
    assert!(stderr.contains("E_POLICY_DENIED"), "{stderr}");

    // 3. A tag as a target: not served.
    let line = ["push", "origin", "HEAD:refs/land/tags/v1"];
    let tagged = site.git_as(AGENT, &work, &line);
    let stderr = text(&tagged.stderr);
    assert!(!tagged.status.success(), "{stderr}");
    assert!(stderr.contains("[remote rejected]"), "{stderr}");
    assert!(stderr.contains("not served"), "{stderr}");

    // 4. Nothing landed: main where it was, no land on it.
    assert_eq!(site.main_head(), head);
    let keys = site.land_keys();
    assert!(keys.iter().all(Option::is_none), "{keys:?}");
}

/// Criterion 4: [`PUSHERS`] clients push to `refs/land/main` while [`LANDERS`] agents run
/// `land`: each change lands exactly once, and the store verifies clean; each push again
/// replays, writing nothing.
#[test]
fn land_push_concurrent_with_agents() {
    let site = Site::new();

    // 1. Each pusher's clone and commit, each agent's change, made one after another.
    let pushers: Vec<(String, PathBuf, String)> = (0..PUSHERS)
        .map(|n| {
            let who = format!("person/p{n}");
            let dir = site.clone(&format!("p{n}"));
            let tip = site.commit(&dir, &format!("pushed-{n}.txt"), &format!("pusher {n}\n"));
            (who, dir, tip)
        })
        .collect();
    let changes: Vec<(String, String)> = (0..LANDERS)
        .map(|n| {
            let path = site.work.path().join(format!("change-{n}.json"));
            let put = format!(
                r#"[{{"path": "agent-{n}.txt", "op": "put", "content": {{"inline": "agent {n}\n"}}}}]"#
            );
            fs::write(&path, put).unwrap();
            let who = support::agent(n);
            let args = ["commit", "-m", "agent", "--changes", path.to_str().unwrap()];
            let change = site.so_as(&who, &args).ok()["changeset"]
                .as_str()
                .unwrap()
                .to_owned();
            (who, change)
        })
        .collect();

    // 2. Every push and every land at once.
    thread::scope(|scope| {
        for (who, dir, _) in &pushers {
            let site = &site;
            scope.spawn(move || {
                let pushed = site.git_as(who, dir, &["push", "origin", LAND_MAIN]);
                assert!(pushed.status.success(), "{who}: {}", text(&pushed.stderr));
            });
        }
        for (n, (who, change)) in changes.iter().enumerate() {
            let site = &site;
            scope.spawn(move || {
                let key = format!("agent-land-{n}");
                let landed = site.so_as(who, &["land", change, "--key", &key]).ok();
                assert_eq!(landed["status"], "landed", "{landed}");
            });
        }
    });

    // 3. Each key once on main's first parents, every change there, the store clean.
    let keys: Vec<String> = site.land_keys().into_iter().flatten().collect();
    let pushed = pushers.iter().map(|(who, _, tip)| key_of(who, tip));
    let agents = (0..LANDERS).map(|n| format!("agent-land-{n}"));
    for key in pushed.chain(agents) {
        let found = keys.iter().filter(|k| **k == key).count();
        assert_eq!(found, 1, "{key} in {keys:?}");
    }
    assert_eq!(keys.len(), PUSHERS + LANDERS, "{keys:?}");
    site.assert_clean();

    // 4. Each push again: replayed, nothing written.
    let journal = site.journal_len();
    for (who, dir, _) in &pushers {
        let pushed = site.git_as(who, dir, &["push", "origin", LAND_MAIN]);
        assert!(pushed.status.success(), "{who}: {}", text(&pushed.stderr));
    }
    assert_eq!(site.journal_len(), journal, "a replay wrote");
}
