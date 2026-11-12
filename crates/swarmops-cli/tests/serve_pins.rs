//! The refusals of `swarmops serve`'s pushes and lands that no other test holds (M14 design §6,
//! §7; M16 design §7): a deletion whose old value is stale deletes nothing, a tag moved backward
//! is refused as immutable whatever the caller's grants, a tag is deleted with the `force` grant
//! only, a deletion of `refs/land/<target>` is refused, and a refused land writes no frame to
//! the journal.
//!
//! Stock git never sends a deletion whose old value the advertisement did not hold, nor one of a
//! ref the remote did not advertise, so those two are spoken by hand to `git-receive-pack`
//! through the test kit's forced-command stand-in ([`Site::raw_delete`]).

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Instant;

use serde_json::Value;
use swarmops_testkit::child::{CHILD_TIMEOUT, Child};
use swarmops_testkit::git::fixture;
use swarmops_testkit::serve::ServeSite;
use swarmops_testkit::store::layout_of;
use tempfile::TempDir;

use crate::support::{self, Swarmops};

/// The agent a forced command names: pushes and lands, forces the feature branches only.
const AGENT: &str = "person/ada";

/// An agent that pushes the tags and holds `force` on them.
const TAGGER: &str = "person/bob";

/// An agent that only reads.
const READER: &str = "person/eve";

/// The agent that writes the served store directly, through the command line.
const WRITER: &str = "agent/worker-1";

/// The store every site serves.
const STORE: &str = "acme";

/// The served store's access file: [`AGENT`] pushes `main`, the release and feature branches
/// and the tags, forces the feature branches, and lands onto `main` and the release branches;
/// [`TAGGER`] pushes and forces the tags; [`READER`] reads.
const ACCESS: &str = r#"version = 1

[[grant]]
who = ["person/ada"]
push = ["main", "release/*", "feature/**", "tags/**"]
force = ["feature/**"]
land = ["main", "release/*"]

[[grant]]
who = ["person/bob"]
push = ["tags/**"]
force = ["tags/**"]

[[grant]]
who = ["person/eve"]
read = true
"#;

/// The feature branch the pushes move and delete, in git and in the store.
const FEATURE_REF: &str = "refs/heads/feature/x";
const FEATURE: &str = "feature/x";

/// The tag the pushes move and delete, in git and in the store.
const TAG_REF: &str = "refs/tags/v1.4.0";
const TAG: &str = "tags/v1.4.0";

/// The release branch a land names, in the store and as a land's target.
const RELEASE: &str = "release/1";
const LAND_RELEASE: &str = "refs/land/release/1";

/// Where a push names `main` as the branch it lands onto.
const LAND_MAIN: &str = "refs/land/main";

/// What `proc-receive` answers a command whose old value is not its ref's export now.
const STALE: &str = "stale: refs/heads/feature/x moved; fetch first";

/// Why a deletion of a land ref is refused.
const NOTHING_TO_DELETE: &str = "lands a commit: there is nothing to delete";

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
        let linear = fixture("linear");
        let source = linear.repo.as_deref().unwrap().to_str().unwrap();
        site.so(&["git", "import", source, "--ref", "HEAD", "--into", "main"]);
        // Written after the import, so that no preparation runs beside the test's connections.
        fs::write(site.served.store(STORE).join("access.toml"), ACCESS).unwrap();
        site
    }

    /// The store's path.
    fn store(&self) -> String {
        self.served.store(STORE).to_str().unwrap().to_owned()
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

    /// The length of the store's journal.
    fn journal_len(&self) -> u64 {
        let log = layout_of(self.store()).journal_log();
        fs::metadata(log).unwrap().len()
    }

    /// Every ref of `serve.git`, one `<oid> <name>` line each.
    fn served_refs(&self) -> String {
        let output = self
            .served
            .command()
            .arg("--git-dir")
            .arg(self.served.store(STORE).join("git/serve.git"))
            .args(["for-each-ref", "--format=%(objectname) %(refname)"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", text(&output.stderr));
        text(&output.stdout)
    }

    /// Whether `serve.git` has `name` at `oid`.
    fn serves(&self, oid: &str, name: &str) -> bool {
        self.served_refs()
            .lines()
            .any(|line| line == format!("{oid} {name}"))
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

    /// Everything a refused land must leave as it was: the journal's length, the store's refs
    /// and `serve.git`'s.
    fn unwritten(&self) -> (u64, Value, String) {
        let (store, served) = self.refs();
        (self.journal_len(), store, served)
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

    /// git with `args` in `dir` as `who`, which must succeed; its stdout.
    fn git_ok(&self, who: &str, dir: &Path, args: &[&str]) -> String {
        let output = self.git_as(who, dir, args);
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            text(&output.stderr)
        );
        text(&output.stdout)
    }

    /// git with `args` in `dir` as [`AGENT`], which must succeed; its stdout.
    fn git(&self, dir: &Path, args: &[&str]) -> String {
        self.git_ok(AGENT, dir, args)
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

    /// One deletion spoken by hand to `git-receive-pack` through `who`'s forced command: the
    /// command `<old> <zero> <full>` with `report-status`, a flush, and no pack, as git sends a
    /// push that only deletes. What the connection gave back.
    fn raw_delete(&self, who: &str, old: &str, full: &str) -> Output {
        // 1. The command as one pkt-line, then a flush.
        let zero = "0".repeat(old.len());
        let line = format!("{old} {zero} {full}\0report-status\n");
        let input = format!("{:04x}{line}0000", line.len() + 4);

        // 2. The forced command, as ssh would start it; receive-pack's advertisement is small
        //    enough for the pipe, so the input goes in whole before any of it is read.
        let ssh = self.served.ssh_command(who);
        let line = format!("{ssh} git@remote.invalid \"git-receive-pack '/{STORE}.git'\"");
        let mut child = Command::new("sh")
            .args(["-c", &line])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let output = Child::adopt(child).wait(Instant::now() + CHILD_TIMEOUT);

        // 3. Neither stream holds the store's path.
        let said = said(&output);
        assert!(!said.contains(&self.store()), "a host path: {said}");
        output
    }
}

/// `bytes` as text.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Everything `output` said: its stdout (receive-pack's report), then its stderr (the hooks').
fn said(output: &Output) -> String {
    text(&output.stdout) + &text(&output.stderr)
}

/// git's report of a push refused with `code`: exit non-zero, `[remote rejected]`, `code` in
/// it, and none of `others`.
fn assert_refused(output: &Output, code: &str, others: &[&str]) {
    let stderr = text(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("[remote rejected]"), "{stderr}");
    assert!(stderr.contains(code), "no {code:?} in: {stderr}");
    for other in others {
        assert!(!stderr.contains(other), "{other:?} in: {stderr}");
    }
}

/// A deletion whose old value is stale, because the branch moved after the caller read it,
/// deletes nothing: `proc-receive` answers `ng … stale`, and the branch keeps the newer commit
/// in the store and in `serve.git`. The same command with the value the branch has now deletes
/// it, so the refusal is the old value's.
#[test]
fn serve_push_refuses_a_stale_delete() {
    let site = Site::new();
    let work = site.clone("work");
    let stale = site.rev(&work, "HEAD");
    let push = ["push", "--quiet", "origin", "HEAD:refs/heads/feature/x"];

    // 1. feature/x at the clone's head, then moved forward.
    site.git(&work, &push);
    let newer = site.commit(&work, "newer.txt");
    site.git(&work, &push);
    let before = site.refs();

    // 2. The deletion from the first value: refused, nothing deleted.
    let refused = site.raw_delete(AGENT, &stale, FEATURE_REF);
    let report = said(&refused);
    assert!(
        report.contains(&format!("ng {FEATURE_REF} {STALE}")),
        "{report}"
    );
    assert!(!report.contains(&format!("ok {FEATURE_REF}")), "{report}");
    assert_eq!(site.refs(), before);
    assert!(site.serves(&newer, FEATURE_REF), "{}", site.served_refs());

    // 3. The deletion from the value it has now: deleted from the store and from serve.git.
    let deleted = site.raw_delete(AGENT, &newer, FEATURE_REF);
    let report = said(&deleted);
    assert!(report.contains(&format!("ok {FEATURE_REF}")), "{report}");
    assert!(!site.ref_names().contains(&FEATURE.to_owned()));
    assert!(
        !site.served_refs().contains(FEATURE_REF),
        "{}",
        site.served_refs()
    );
    site.so(&["verify", "--deep"]);
}

/// A tag moved backward, to a commit its own reaches, is refused `E_TAG_IMMUTABLE` and never
/// `E_POLICY_DENIED`: by a caller without the `force` grant on it, whom a branch's backward
/// move would be denied, and by one with it, whom a branch's would be allowed. Nothing moves.
#[test]
fn serve_push_backward_tag_is_immutable() {
    let site = Site::new();
    let work = site.clone("work");
    let tip = site.rev(&work, "HEAD");

    // 1. The tag at the clone's head.
    site.git(&work, &["tag", "v1.4.0"]);
    site.git(&work, &["push", "--quiet", "origin", TAG_REF]);
    assert!(site.serves(&tip, TAG_REF), "{}", site.served_refs());
    let before = site.refs();

    // 2. The tag moved back to the head's parent, forced by the client: refused as immutable
    //    for the caller without force and for the one with it.
    site.git(&work, &["tag", "--force", "v1.4.0", "HEAD~1"]);
    for who in [AGENT, TAGGER] {
        let moved = site.git_as(who, &work, &["push", "--force", "origin", TAG_REF]);
        assert_refused(&moved, "E_TAG_IMMUTABLE", &["E_POLICY_DENIED"]);
        assert_eq!(site.refs(), before, "{who}");
    }
    assert!(site.serves(&tip, TAG_REF), "{}", site.served_refs());
}

/// A tag is deleted with the `force` grant on it: without, the deletion is refused
/// `E_POLICY_DENIED` and the tag stays; with it, the tag leaves the store and `serve.git`, and
/// the same name is created again at another commit.
#[test]
fn serve_push_deletes_a_tag() {
    let site = Site::new();
    let work = site.clone("work");
    let tip = site.rev(&work, "HEAD");
    let delete = ["push", "origin", ":refs/tags/v1.4.0"];

    // 1. The tag at the clone's head.
    site.git(&work, &["tag", "v1.4.0"]);
    site.git(&work, &["push", "--quiet", "origin", TAG_REF]);
    let before = site.refs();

    // 2. Without force: refused, the tag where it was.
    let denied = site.git_as(AGENT, &work, &delete);
    assert_refused(&denied, "E_POLICY_DENIED", &[]);
    assert_eq!(site.refs(), before);
    assert!(site.serves(&tip, TAG_REF), "{}", site.served_refs());

    // 3. With force: deleted from the store and from serve.git, no other ref touched.
    site.git_ok(TAGGER, &work, &delete);
    assert!(!site.ref_names().contains(&TAG.to_owned()));
    assert!(
        !site.served_refs().contains(TAG_REF),
        "{}",
        site.served_refs()
    );
    assert!(
        site.serves(&tip, "refs/heads/main"),
        "{}",
        site.served_refs()
    );

    // 4. The name is free again: created at the head's parent.
    let parent = site.rev(&work, "HEAD~1");
    site.git(&work, &["tag", "--force", "v1.4.0", "HEAD~1"]);
    site.git(&work, &["push", "--quiet", "origin", TAG_REF]);
    assert!(site.serves(&parent, TAG_REF), "{}", site.served_refs());
    site.so(&["verify", "--deep"]);
}

/// A deletion of `refs/land/<target>` is refused `E_INVALID_ARG`, since a land ref names a
/// commit to land and holds nothing to delete: for `main` and for a release branch the caller
/// may land onto, with the target's own commit as the old value. The target keeps its commit,
/// and the journal gains no frame.
#[test]
fn serve_land_refuses_a_deletion() {
    let site = Site::new();
    let work = site.clone("work");
    let head = site.rev(&work, "HEAD");

    // 1. release/1 beside main, served: both name the clone's head.
    site.so(&["ref", "set", RELEASE, "main", "--create"]);
    site.git(&work, &["fetch", "--quiet", "origin"]);
    assert_eq!(site.rev(&work, "origin/release/1"), head);
    let before = site.unwritten();

    // 2. Each land ref deleted: refused, the reason said, nothing written.
    for land in [LAND_MAIN, LAND_RELEASE] {
        let refused = site.raw_delete(AGENT, &head, land);
        let report = said(&refused);
        assert!(report.contains(&format!("ng {land} ")), "{report}");
        assert!(!report.contains(&format!("ok {land}")), "{report}");
        assert!(report.contains("E_INVALID_ARG"), "{report}");
        assert!(report.contains(NOTHING_TO_DELETE), "{report}");
        assert_eq!(site.unwritten(), before, "{land}: {report}");
    }

    // 3. Both targets still served at the head.
    assert!(
        site.serves(&head, "refs/heads/main"),
        "{}",
        site.served_refs()
    );
    assert!(
        site.serves(&head, "refs/heads/release/1"),
        "{}",
        site.served_refs()
    );
    site.so(&["verify", "--deep"]);
}

/// A land refused before it lands writes no frame: the journal's length, the store's refs and
/// `serve.git`'s are the same before and after a land by a caller without the `land` grant, a
/// land onto a tag, a deletion of the land ref, and a land of the tip the target holds already.
/// The first three are refused before the pushed commit is imported, so their commit never
/// reaches the store.
#[test]
fn serve_land_refusals_write_no_frame() {
    let site = Site::new();
    let work = site.clone("work");
    let head = site.rev(&work, "HEAD");
    let before = site.unwritten();

    // 1. The tip main holds already: nothing to land.
    let nothing = site.git_as(AGENT, &work, &["push", "origin", "HEAD:refs/land/main"]);
    assert_refused(&nothing, "E_NOTHING_TO_LAND", &[]);
    assert_eq!(site.unwritten(), before, "nothing to land");

    // 2. A new commit, by a caller without the land grant.
    site.commit(&work, "denied.txt");
    let denied = site.git_as(READER, &work, &["push", "origin", "HEAD:refs/land/main"]);
    assert_refused(&denied, "E_POLICY_DENIED", &[]);
    assert_eq!(site.unwritten(), before, "no land grant");

    // 3. The same commit onto a tag: not served.
    let line = ["push", "origin", "HEAD:refs/land/tags/v1"];
    let tagged = site.git_as(AGENT, &work, &line);
    assert_refused(&tagged, "not served", &[]);
    assert_eq!(site.unwritten(), before, "a tag as a target");

    // 4. The land ref deleted.
    let deleted = site.raw_delete(AGENT, &head, LAND_MAIN);
    let report = said(&deleted);
    assert!(report.contains(NOTHING_TO_DELETE), "{report}");
    assert_eq!(site.unwritten(), before, "a land ref deleted");
    site.so(&["verify", "--deep"]);
}
