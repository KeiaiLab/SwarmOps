//! Pushes to `swarmops serve` (M14 design §2.4, §6), as stock git makes them over ssh through the
//! test kit's forced-command stand-in: receive-pack hands every command to the `proc-receive`
//! hook, which imports the pushed commits from `serve.git` and moves the store ref by
//! compare-and-swap, as the caller `--as` names. The hook runs only inside a push `serve`
//! started, and a push it cannot decide moves nothing. The push rules: branches and lightweight
//! tags created, a tag never moved, `main` never deleted and moved only forward along first
//! parents, another branch's backward move and every deletion by the `force` grant, a stale old
//! value refused, each command of a push decided on its own.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Instant;

use serde_json::Value;
use swarmops_testkit::child::{CHILD_TIMEOUT, Child};
use swarmops_testkit::git::fixture;
use swarmops_testkit::serve::ServeSite;
use tempfile::TempDir;

use crate::support::{self, Swarmops};

/// The agent a forced command names.
const AGENT: &str = "person/ada";

/// An agent that pushes feature branches, without `force`.
const PUSHER: &str = "person/bob";

/// An agent that only reads.
const READER: &str = "person/eve";

/// An agent the serving account's environment names, which never pushes.
const INTRUDER: &str = "agent/intruder";

/// The agent that writes the served store directly, through the command line.
const WRITER: &str = "agent/worker-1";

/// The store every site serves.
const STORE: &str = "acme";

/// The served store's access file: [`AGENT`] pushes `main`, the release and feature branches
/// and the tags, and forces the feature branches; [`PUSHER`] pushes the feature branches;
/// [`READER`] reads.
const ACCESS: &str = r#"version = 1

[[grant]]
who = ["person/ada"]
push = ["main", "release/*", "feature/**", "tags/**"]
force = ["feature/**"]

[[grant]]
who = ["person/bob"]
push = ["feature/**"]

[[grant]]
who = ["person/eve"]
read = true
"#;

/// The most commands one push may hold (`MAX_PUSH_COMMANDS`).
const MAX_PUSH_COMMANDS: usize = 1_000;

/// The variables the parent `serve` gives its hook.
const SERVE_STORE: &str = "SWARMOPS_SERVE_STORE";
const SERVE_IDENTITY: &str = "SWARMOPS_SERVE_IDENTITY";

/// What the hook is asked to run as.
const HOOK: [&str; 3] = ["serve", "--hook", "proc-receive"];

/// receive-pack's side of an empty push to the hook: the version line, a flush, no command, a
/// flush.
const EMPTY_PUSH: &[u8] = b"000eversion=1\n00000000";

/// The hook's answer to it: its version line, a flush, no report, a flush.
const EMPTY_ANSWER: &[u8] = b"000eversion=1\n00000000";

/// A served root with the store [`STORE`] holding the `linear` fixture as `main`, under
/// [`ACCESS`], and a directory for clients.
struct Site {
    /// The root, its stand-ins and the client git.
    served: ServeSite,
    /// The clients' working directories.
    work: TempDir,
}

impl Site {
    /// The store, served by `served`.
    fn on(served: ServeSite) -> Site {
        served.init(STORE);
        Site::filled(served)
    }

    /// The store made at FORMAT 1, served with the test kit's environment.
    fn format1() -> Site {
        let served = ServeSite::new::<Swarmops>();
        let path = served.store(STORE);
        support::run(&["init", "--format", "1", path.to_str().unwrap()], &[]).ok();
        Site::filled(served)
    }

    /// The store `served` made, given [`ACCESS`] and the `linear` fixture as `main`.
    fn filled(served: ServeSite) -> Site {
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

    /// The store, served with the test kit's environment.
    fn new() -> Site {
        Site::on(ServeSite::new::<Swarmops>())
    }

    /// The store's path.
    fn store(&self) -> String {
        self.served.store(STORE).to_str().unwrap().to_owned()
    }

    /// The store's `serve.git`.
    fn serve_git(&self) -> PathBuf {
        self.served.store(STORE).join("git/serve.git")
    }

    /// `args` on the store as [`WRITER`]; its result.
    fn so(&self, args: &[&str]) -> Value {
        let store = self.store();
        let line: Vec<&str> = ["--repo", &store, "--as", WRITER]
            .iter()
            .chain(args)
            .copied()
            .collect();
        support::run(&line, &[]).ok()
    }

    /// The store's newest operation.
    fn newest_op(&self) -> Value {
        self.so(&["op", "log", "-n", "1"])["ops"][0].clone()
    }

    /// Every ref of `serve.git`, one `<oid> <name>` line each.
    fn served_refs(&self) -> String {
        let output = self
            .served
            .command()
            .arg("--git-dir")
            .arg(self.serve_git())
            .args(["for-each-ref", "--format=%(objectname) %(refname)"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", text(&output.stderr));
        text(&output.stdout)
    }

    /// git with `args` in `dir`, its ssh the stand-in `ssh`; what it did.
    fn git_via(&self, ssh: &str, dir: &Path, args: &[&str]) -> Output {
        self.served
            .command()
            .env("GIT_SSH_COMMAND", ssh)
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap()
    }

    /// git with `args` in `dir` as [`AGENT`]; what it did. Its stderr never holds the store's
    /// path.
    fn git_out(&self, dir: &Path, args: &[&str]) -> Output {
        self.git_as(AGENT, dir, args)
    }

    /// git with `args` in `dir` as `who`; what it did. Its stderr never holds the store's path.
    fn git_as(&self, who: &str, dir: &Path, args: &[&str]) -> Output {
        let output = self.git_via(&self.served.ssh_command(who), dir, args);
        let stderr = text(&output.stderr);
        assert!(!stderr.contains(&self.store()), "a host path: {stderr}");
        output
    }

    /// The store's refs and `serve.git`'s, to see that nothing moved.
    fn refs(&self) -> (Value, String) {
        (
            self.so(&["ref", "list"])["refs"].clone(),
            self.served_refs(),
        )
    }

    /// The names of the store's refs.
    fn ref_names(&self) -> Vec<String> {
        let listed = self.so(&["ref", "list"]);
        (listed["refs"].as_array().unwrap().iter())
            .map(|entry| entry["name"].as_str().unwrap().to_owned())
            .collect()
    }

    /// Whether `serve.git` has `name` at `oid`.
    fn serves(&self, oid: &str, name: &str) -> bool {
        self.served_refs()
            .lines()
            .any(|line| line == format!("{oid} {name}"))
    }

    /// git with `args` in `dir` as [`AGENT`], which must succeed; its stdout.
    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let output = self.git_out(dir, args);
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            text(&output.stderr)
        );
        text(&output.stdout)
    }

    /// The store cloned into `name`; the clone's directory.
    fn clone(&self, name: &str) -> PathBuf {
        let url = self.served.ssh_url(STORE);
        self.git(self.work.path(), &["clone", "--quiet", &url, name]);
        self.work.path().join(name)
    }

    /// A commit of `path` in the clone `dir`, on its branch; its id.
    fn commit(&self, dir: &Path, path: &str) -> String {
        fs::write(dir.join(path), format!("{path}\n")).unwrap();
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
}

/// `bytes` as text.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// git's report of a push the hook refused: exit non-zero, `[remote rejected]`, and each of
/// `needles` in it.
fn assert_rejected(output: &Output, needles: &[&str]) {
    let stderr = text(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("[remote rejected]"), "{stderr}");
    for needle in needles {
        assert!(stderr.contains(needle), "no {needle:?} in: {stderr}");
    }
}

/// The hook run directly in `dir` with `env`, `args` after [`HOOK`] and `input` on its stdin;
/// what it did.
fn hook(dir: &Path, args: &[&str], env: &[(&str, &str)], input: &[u8]) -> Output {
    let line: Vec<&str> = HOOK.iter().chain(args).copied().collect();
    let mut command = support::swarmops(&line, env);
    let mut child = command
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    Child::adopt(child).wait(Instant::now() + CHILD_TIMEOUT)
}

/// The hook's refusal in `output`: exit 2, and `not_from_git` in the envelope on stderr.
fn assert_not_from_git(output: &Output) {
    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    let envelope: Value = serde_json::from_str(&stderr)
        .unwrap_or_else(|e| panic!("the envelope on stderr ({e}): {stderr}"));
    assert_eq!(envelope["error"]["code"], "E_INVALID_ARG", "{stderr}");
    assert_eq!(
        envelope["error"]["details"]["reason"], "not_from_git",
        "{stderr}"
    );
}

/// A bare repository of its own in `dir`, which is no store's `serve.git`.
fn other_bare(site: &Site, dir: &Path) -> PathBuf {
    let bare = dir.join("other.git");
    let output = site
        .served
        .command()
        .args(["init", "--quiet", "--bare"])
        .arg(&bare)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output.stderr));
    bare
}

/// The hook refuses `not_from_git` unless its parent's variables name the store and `GIT_DIR`
/// is that store's `serve.git`.
#[test]
fn hook_refuses_without_the_parent() {
    let site = Site::new();
    site.git(
        site.work.path(),
        &["ls-remote", &site.served.ssh_url(STORE)],
    );
    let store = site.store();
    let serve_git = site.serve_git();
    let serve_git = serve_git.to_str().unwrap();
    let other = other_bare(&site, site.work.path());
    let other = other.to_str().unwrap();

    // 1. No variable; a caller but no store; a store but no GIT_DIR; another repository.
    let cases: [&[(&str, &str)]; 4] = [
        &[("GIT_DIR", serve_git)],
        &[(SERVE_IDENTITY, AGENT), ("GIT_DIR", serve_git)],
        &[(SERVE_IDENTITY, AGENT), (SERVE_STORE, &store)],
        &[
            (SERVE_IDENTITY, AGENT),
            (SERVE_STORE, &store),
            ("GIT_DIR", other),
        ],
    ];
    for env in cases {
        assert_not_from_git(&hook(site.work.path(), &[], env, EMPTY_PUSH));
    }
}

/// receive-pack runs the hook in the bare `serve.git` with `GIT_DIR` `.`: taken as the store's,
/// it answers the handshake; the same `.` in another bare repository is refused.
#[test]
fn hook_accepts_relative_git_dir() {
    let site = Site::new();
    site.git(
        site.work.path(),
        &["ls-remote", &site.served.ssh_url(STORE)],
    );
    let store = site.store();
    let env = [
        (SERVE_IDENTITY, AGENT),
        (SERVE_STORE, store.as_str()),
        ("GIT_DIR", "."),
    ];

    // 1. In serve.git: the handshake answered, no report, exit 0.
    let accepted = hook(&site.serve_git(), &[], &env, EMPTY_PUSH);
    assert!(accepted.status.success(), "{}", text(&accepted.stderr));
    assert_eq!(accepted.stdout, EMPTY_ANSWER, "{}", text(&accepted.stdout));

    // 2. In another bare repository: refused.
    let other = other_bare(&site, site.work.path());
    assert_not_from_git(&hook(&other, &[], &env, EMPTY_PUSH));
}

/// The hook's caller is its parent's alone: `SWARMOPS_AGENT` names none, and `--as` on the hook's
/// line is refused.
#[test]
fn hook_ignores_agent_env() {
    let site = Site::new();
    site.git(
        site.work.path(),
        &["ls-remote", &site.served.ssh_url(STORE)],
    );
    let store = site.store();

    // 1. SWARMOPS_AGENT without the parent's caller.
    let agent_env = [
        ("SWARMOPS_AGENT", AGENT),
        (SERVE_STORE, store.as_str()),
        ("GIT_DIR", "."),
    ];
    assert_not_from_git(&hook(&site.serve_git(), &[], &agent_env, EMPTY_PUSH));

    // 2. --as on the hook's line, beside the parent's caller.
    let parent_env = [
        (SERVE_IDENTITY, AGENT),
        (SERVE_STORE, store.as_str()),
        ("GIT_DIR", "."),
    ];
    let line = ["--as", AGENT];
    assert_not_from_git(&hook(&site.serve_git(), &line, &parent_env, EMPTY_PUSH));
}

/// A fast-forward pushed to `main` moves the store's `main` to the pushed commit's changeset,
/// byte for byte (a fresh clone has the pushed id), recorded for the caller `--as` names, not
/// for the serving account's `SWARMOPS_AGENT`; a new release branch is created.
#[test]
fn serve_push_fast_forwards_main() {
    let served = ServeSite::new::<Swarmops>().with_env(&[("SWARMOPS_AGENT", INTRUDER)]);
    let site = Site::on(served);
    let work = site.clone("work");
    let pushed = site.commit(&work, "pushed.txt");

    // 1. The push of main, a fast-forward: accepted.
    site.git(&work, &["push", "--quiet", "origin", "main"]);

    // 2. The store's main moved, by the pusher.
    let op = site.newest_op();
    let moved = &op["ref_moves"][0];
    assert_eq!(moved["name"], "main", "{op}");
    assert_eq!(moved["actor"], AGENT, "{op}");
    let head = moved["new"].as_str().unwrap();
    let shown = site.so(&["show", head]);
    assert_eq!(shown["metadata"]["git.oid"], pushed.as_str(), "{shown}");

    // 3. Byte for byte: serve.git and a fresh clone have the pushed id.
    assert!(
        site.served_refs()
            .contains(&format!("{pushed} refs/heads/main")),
        "{}",
        site.served_refs()
    );
    let fresh = site.clone("fresh");
    assert_eq!(site.rev(&fresh, "origin/main"), pushed);

    // 4. A new release branch: created where the push puts it.
    let release = site.commit(&work, "release.txt");
    site.git(
        &work,
        &["push", "--quiet", "origin", "HEAD:refs/heads/release/2"],
    );
    let listed = site.so(&["ref", "list"]);
    let names: Vec<&str> = listed["refs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"release/2"), "{listed}");
    assert!(
        site.served_refs()
            .contains(&format!("{release} refs/heads/release/2")),
        "{}",
        site.served_refs()
    );
}

/// A commit holding a gitlink is refused by the import's lossy rules: nothing moves, and its
/// objects, which git moved into `serve.git` before the hook ran, are not fetched by id over
/// protocol v0 (`uploadpack.allowAnySHA1InWant=false`). Protocol v2 takes a want of any object
/// the repository holds (gitprotocol-v2, `fetch`), so there the refused tip is fetched: an open
/// item of the M14 design's risk R-2, not asserted here.
#[test]
fn serve_push_refuses_a_gitlink() {
    let site = Site::new();
    let work = site.clone("work");
    let before = (site.newest_op()["op"].clone(), site.served_refs());

    // 1. A commit with a submodule's gitlink, pushed: refused by the hook.
    let linked = site.rev(&work, "HEAD");
    let entry = format!("160000,{linked},sub");
    site.git(&work, &["update-index", "--add", "--cacheinfo", &entry]);
    site.git(&work, &["commit", "--quiet", "-m", "gitlink"]);
    let tip = site.rev(&work, "HEAD");
    let pushed = site.git_out(&work, &["push", "origin", "main"]);
    let stderr = text(&pushed.stderr);
    assert!(!pushed.status.success(), "{stderr}");
    assert!(stderr.contains("[remote rejected]"), "{stderr}");
    assert!(stderr.contains("E_GIT_IMPORT_LOSSY"), "{stderr}");

    // 2. Nothing moved: the store's journal and serve.git's refs as they were.
    let after = (site.newest_op()["op"].clone(), site.served_refs());
    assert_eq!(after, before);

    // 3. The refused tip is not fetched by id, from a clone that lacks it.
    let fresh = site.clone("fresh");
    let line = ["-c", "protocol.version=0", "fetch", "origin", &tip];
    let fetched = site.git_out(&fresh, &line);
    assert!(!fetched.status.success(), "{}", text(&fetched.stderr));
}

/// receive-pack moves no ref itself: with the hook's program gone, a push is rejected, and
/// neither `serve.git` nor the store changes.
#[test]
fn hook_unrunnable_moves_nothing() {
    let site = Site::new();
    let work = site.clone("work");
    site.commit(&work, "unrun.txt");
    let before = (site.newest_op()["op"].clone(), site.served_refs());

    // 1. A stand-in that runs a copy of the binary, which the client's pre-push hook removes:
    //    serve runs, then the hook it names cannot.
    let dir = TempDir::new().unwrap();
    let copy = dir.path().join("swarmops-copy");
    fs::copy(support::BIN, &copy).unwrap();
    let ssh = site.served.ssh_command(AGENT);
    let script = fs::read_to_string(ssh.trim_matches('\'')).unwrap();
    let script = script.replace(support::BIN, copy.to_str().unwrap());
    let stand_in = dir.path().join("ssh");
    fs::write(&stand_in, script).unwrap();
    fs::set_permissions(&stand_in, fs::Permissions::from_mode(0o755)).unwrap();
    let pre_push = work.join(".git/hooks/pre-push");
    let remove = format!("#!/bin/sh\nrm -f '{}'\n", copy.to_str().unwrap());
    fs::write(&pre_push, remove).unwrap();
    fs::set_permissions(&pre_push, fs::Permissions::from_mode(0o755)).unwrap();

    // 2. The push: rejected, the hook not run. sh names the hook file in its error, so this
    //    run's stderr is not checked for the store's path.
    let stand_in = stand_in.to_str().unwrap();
    let pushed = site.git_via(stand_in, &work, &["push", "origin", "main"]);
    let stderr = text(&pushed.stderr);
    assert!(!pushed.status.success(), "{stderr}");
    assert!(!copy.exists(), "the pre-push hook ran");
    assert!(stderr.contains("[remote rejected]"), "{stderr}");
    assert!(stderr.contains("proc-receive"), "{stderr}");

    // 3. Nothing moved.
    let after = (site.newest_op()["op"].clone(), site.served_refs());
    assert_eq!(after, before);
}

/// receive-pack advertises neither `atomic` nor `push-options`, so `git push --atomic` fails on
/// the client before it sends anything.
#[test]
fn serve_push_advertises_no_atomic() {
    let site = Site::new();
    let work = site.clone("work");
    site.commit(&work, "atomic.txt");
    let before = site.newest_op()["op"].clone();

    // 1. The advertisement, traced.
    let traced = site
        .served
        .command()
        .env("GIT_SSH_COMMAND", site.served.ssh_command(AGENT))
        .env("GIT_TRACE_PACKET", "1")
        .current_dir(&work)
        .args(["push", "--dry-run", "origin", "main"])
        .output()
        .unwrap();
    let trace = text(&traced.stderr);
    assert!(traced.status.success(), "{trace}");
    let caps: Vec<&str> = trace
        .lines()
        .filter(|line| line.contains("report-status"))
        .collect();
    assert!(!caps.is_empty(), "{trace}");
    for line in caps {
        assert!(!line.contains(" atomic"), "{line}");
        assert!(!line.contains("push-options"), "{line}");
    }

    // 2. --atomic: refused by the client, nothing sent.
    let atomic = site.git_out(&work, &["push", "--atomic", "origin", "main"]);
    let stderr = text(&atomic.stderr);
    assert!(!atomic.status.success(), "{stderr}");
    assert!(stderr.contains("atomic"), "{stderr}");
    assert_eq!(site.newest_op()["op"], before);
}

/// A FORMAT 2 branch and a lightweight tag are created (`v1.4.0` as `tags/v1.4.0`); a tag never
/// moves (`E_TAG_IMMUTABLE`), and an annotated tag, which is no changeset, is refused
/// (`annotated_tag`): neither moves anything.
#[test]
fn serve_push_creates_branches_and_tags() {
    let site = Site::new();
    let work = site.clone("work");
    let tip = site.commit(&work, "feature.txt");

    // 1. feature/x: created in the store and in serve.git.
    site.git(
        &work,
        &["push", "--quiet", "origin", "HEAD:refs/heads/feature/x"],
    );
    assert!(site.ref_names().contains(&"feature/x".to_owned()));
    assert!(
        site.serves(&tip, "refs/heads/feature/x"),
        "{}",
        site.served_refs()
    );

    // 2. The tag v1.4.0: created as tags/v1.4.0.
    site.git(&work, &["tag", "v1.4.0"]);
    site.git(&work, &["push", "--quiet", "origin", "refs/tags/v1.4.0"]);
    assert!(site.ref_names().contains(&"tags/v1.4.0".to_owned()));
    assert!(
        site.serves(&tip, "refs/tags/v1.4.0"),
        "{}",
        site.served_refs()
    );

    // 3. The tag moved, forced by the client: refused by the store.
    let before = site.refs();
    site.commit(&work, "moved.txt");
    site.git(&work, &["tag", "--force", "v1.4.0"]);
    let moved = site.git_out(&work, &["push", "--force", "origin", "refs/tags/v1.4.0"]);
    assert_rejected(&moved, &["E_TAG_IMMUTABLE"]);
    assert_eq!(site.refs(), before);

    // 4. An annotated tag: refused before its import.
    site.git(&work, &["tag", "--annotate", "--message", "v2", "v2.0.0"]);
    let annotated = site.git_out(&work, &["push", "origin", "refs/tags/v2.0.0"]);
    assert_rejected(&annotated, &["E_INVALID_ARG", "annotated tag"]);
    assert_eq!(site.refs(), before);
}

/// A deletion needs the `force` grant (`E_POLICY_DENIED` without it), and `main` is never
/// deleted (`E_REF_PROTECTED`), whoever asks.
#[test]
fn serve_push_deletes_with_force() {
    let site = Site::new();
    let work = site.clone("work");
    let main = site.rev(&work, "HEAD");
    site.git(
        &work,
        &["push", "--quiet", "origin", "HEAD:refs/heads/feature/x"],
    );
    let before = site.refs();

    // 1. Without force: refused, nothing deleted.
    let denied = site.git_as(PUSHER, &work, &["push", "origin", ":refs/heads/feature/x"]);
    assert_rejected(&denied, &["E_POLICY_DENIED"]);
    assert_eq!(site.refs(), before);

    // 2. With force: deleted from the store and from serve.git.
    site.git(
        &work,
        &["push", "--quiet", "origin", ":refs/heads/feature/x"],
    );
    assert!(!site.ref_names().contains(&"feature/x".to_owned()));
    assert!(
        !site.served_refs().contains("refs/heads/feature/x"),
        "{}",
        site.served_refs()
    );

    // 3. main: protected.
    let before = site.refs();
    let protected = site.git_out(&work, &["push", "origin", ":refs/heads/main"]);
    assert_rejected(&protected, &["E_REF_PROTECTED"]);
    assert_eq!(site.refs(), before);
    assert!(
        site.serves(&main, "refs/heads/main"),
        "{}",
        site.served_refs()
    );
}

/// Criterion 3: a push whose old value is stale, because another clone pushed between its
/// advertisement and its commands, moves nothing and exits non-zero; the store verifies clean.
#[test]
fn serve_push_refuses_a_stale_old() {
    let site = Site::new();
    let first = site.clone("first");
    let second = site.clone("second");
    let won = site.commit(&first, "first.txt");
    site.commit(&second, "second.txt");

    // 1. The second clone's pre-push hook, which runs after the advertisement, pushes the first
    //    clone's commit to main.
    let pre_push = second.join(".git/hooks/pre-push");
    let race = format!(
        "#!/bin/sh\nunset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_PREFIX\n\
         cd '{}' && exec git push --quiet origin main\n",
        first.to_str().unwrap()
    );
    fs::write(&pre_push, race).unwrap();
    fs::set_permissions(&pre_push, fs::Permissions::from_mode(0o755)).unwrap();

    // 2. The second push: stale, refused.
    let stale = site.git_out(&second, &["push", "origin", "main"]);
    assert_rejected(&stale, &["stale: refs/heads/main moved; fetch first"]);

    // 3. main is the first clone's commit, in the store and in serve.git; the store is whole.
    assert!(
        site.serves(&won, "refs/heads/main"),
        "{}",
        site.served_refs()
    );
    let fresh = site.clone("fresh");
    assert_eq!(site.rev(&fresh, "origin/main"), won);
    site.so(&["verify", "--deep"]);
}

/// `main` moves only forward along first parents: a forced rewrite and a merge whose first
/// parent does not reach `main` are `non-fast-forward`, while that merge moves `feature/x`
/// forward (any parent); a backward move of `feature/x` needs `force`.
#[test]
fn serve_push_moves_main_forward_only() {
    let site = Site::new();
    let work = site.clone("work");
    let main = site.rev(&work, "HEAD");
    site.git(
        &work,
        &["push", "--quiet", "origin", "HEAD:refs/heads/feature/x"],
    );

    // 1. main rewritten, forced by the client: refused.
    let before = site.refs();
    site.git(
        &work,
        &["commit", "--quiet", "--amend", "--message", "rewritten"],
    );
    let forced = site.git_out(
        &work,
        &["push", "--force", "origin", "HEAD:refs/heads/main"],
    );
    assert_rejected(&forced, &["non-fast-forward", "refs/land/main"]);
    assert_eq!(site.refs(), before);

    // 2. A merge whose first parent is a side branch and whose second is main: refused on main.
    site.git(
        &work,
        &["checkout", "--quiet", "-B", "side", &format!("{main}~1")],
    );
    let branched = site.commit(&work, "side.txt");
    site.git(
        &work,
        &["merge", "--quiet", "--no-ff", "--message", "merge", &main],
    );
    let merge = site.rev(&work, "HEAD");
    let refused = site.git_out(&work, &["push", "origin", "HEAD:refs/heads/main"]);
    assert_rejected(&refused, &["non-fast-forward", "refs/land/main"]);
    assert!(
        site.serves(&main, "refs/heads/main"),
        "{}",
        site.served_refs()
    );

    // 3. The same merge onto feature/x: forward through its second parent, accepted.
    site.git(
        &work,
        &["push", "--quiet", "origin", "HEAD:refs/heads/feature/x"],
    );
    assert!(
        site.serves(&merge, "refs/heads/feature/x"),
        "{}",
        site.served_refs()
    );

    // 4. feature/x moved back to the side commit: refused without force, moved with it.
    let back = format!("+{branched}:refs/heads/feature/x");
    let denied = site.git_as(PUSHER, &work, &["push", "origin", &back]);
    assert_rejected(&denied, &["E_POLICY_DENIED"]);
    assert!(
        site.serves(&merge, "refs/heads/feature/x"),
        "{}",
        site.served_refs()
    );
    site.git(&work, &["push", "--quiet", "origin", &back]);
    assert!(
        site.serves(&branched, "refs/heads/feature/x"),
        "{}",
        site.served_refs()
    );
}

/// Names the store does not serve are refused: `refs/notes/` (not served), a branch the store
/// reserves (`remote/…`), and a FORMAT 2 branch on a FORMAT 1 store (`E_FORMAT_REQUIRED`).
#[test]
fn serve_push_refuses_unserved_names() {
    let site = Site::new();
    let work = site.clone("work");
    let before = site.refs();

    // 1. refs/notes/x and refs/heads/remote/o/x: neither is a store ref.
    let notes = site.git_out(&work, &["push", "origin", "HEAD:refs/notes/x"]);
    assert_rejected(&notes, &["E_REF_NAME_INVALID", "is not served"]);
    let reserved = site.git_out(&work, &["push", "origin", "HEAD:refs/heads/remote/o/x"]);
    assert_rejected(&reserved, &["E_REF_NAME_INVALID", "reserves"]);
    assert_eq!(site.refs(), before);

    // 2. A FORMAT 1 store: feature/x needs FORMAT 2.
    let old = Site::format1();
    let work = old.clone("work");
    let before = old.refs();
    let branch = old.git_out(&work, &["push", "origin", "HEAD:refs/heads/feature/x"]);
    assert_rejected(&branch, &["E_FORMAT_REQUIRED"]);
    assert_eq!(old.refs(), before);
}

/// A push of more than `MAX_PUSH_COMMANDS` commands is refused whole: every command `ng`,
/// nothing created.
#[test]
fn serve_push_bounds_its_commands() {
    let site = Site::new();
    let work = site.clone("work");
    let before = site.refs();

    // 1. One command past the bound.
    let specs: Vec<String> = (0..=MAX_PUSH_COMMANDS)
        .map(|n| format!("HEAD:refs/heads/feature/b{n}"))
        .collect();
    let mut args = vec!["push", "origin"];
    args.extend(specs.iter().map(String::as_str));
    let pushed = site.git_out(&work, &args);

    // 2. Every one refused; nothing moved.
    assert_rejected(&pushed, &["E_INVALID_ARG"]);
    let stderr = text(&pushed.stderr);
    let rejected = stderr.matches("[remote rejected]").count();
    assert_eq!(rejected, MAX_PUSH_COMMANDS + 1, "{stderr}");
    assert_eq!(site.refs(), before);
}

/// Criterion 5: an identity with `read` only is refused each ref it pushes
/// (`E_POLICY_DENIED`); and the commands of one push are decided each on its own, so a refused
/// one does not stop another from moving.
#[test]
fn serve_push_decides_each_ref() {
    let site = Site::new();
    let work = site.clone("work");
    let tip = site.commit(&work, "each.txt");
    let before = site.refs();

    // 1. The reader: both refs refused.
    let line = [
        "push",
        "origin",
        "HEAD:refs/heads/main",
        "HEAD:refs/heads/feature/y",
    ];
    let denied = site.git_as(READER, &work, &line);
    assert_rejected(&denied, &["E_POLICY_DENIED"]);
    let stderr = text(&denied.stderr);
    assert_eq!(stderr.matches("E_POLICY_DENIED").count(), 2, "{stderr}");
    assert_eq!(site.refs(), before);

    // 2. One push, one ref refused and the other moved.
    let line = [
        "push",
        "origin",
        "HEAD:refs/heads/main",
        "HEAD:refs/notes/x",
    ];
    let mixed = site.git_out(&work, &line);
    assert_rejected(&mixed, &["refs/notes/x", "is not served"]);
    assert!(
        site.serves(&tip, "refs/heads/main"),
        "{}",
        site.served_refs()
    );
    assert!(
        !site.served_refs().contains("refs/notes/x"),
        "{}",
        site.served_refs()
    );
}
