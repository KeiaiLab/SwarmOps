//! `swarmops git import` and `swarmops git export`, through the binary: a git repository's history
//! brought into a store from the test kit's git fixtures (`swarmops_testkit::git`), which real git
//! made and which no test runs git to read, and a store's history written back into a git
//! repository, read back through the git side's own reader (`swarmops_git::GixSource`); the error
//! each refusal exits with, and what it names; and that neither is an MCP tool, since each works
//! on a repository on the caller's host (the M3 plan's decision 7, the M8 plan's decision 1).

use serde_json::{Value, json};
use swarmops_testkit::git::{GitFixture, Manifest, ManifestCommit, fixture};

use crate::support::{BIN, Run, Store, run_bin};

/// The agent the imports run as.
const AGENT: &str = "agent/worker-17";

/// A git fixture, and the path of its repository as the command line gives it.
struct Source {
    fixture: GitFixture,
    path: String,
}

impl Source {
    /// The fixture `name`, extracted for this test.
    fn new(name: &str) -> Source {
        let fixture = fixture(name);
        let repo = fixture
            .repo
            .as_deref()
            .expect("the fixture holds a repository");
        let path = repo.to_str().expect("UTF-8 temp paths").to_owned();
        Source { fixture, path }
    }

    /// What git's plumbing read from the fixture's repository.
    fn manifest(&self) -> &Manifest {
        self.fixture
            .manifest
            .as_ref()
            .expect("the fixture has a manifest")
    }

    /// The commit the ref `name` (a full ref name, such as `refs/heads/older`) names.
    fn commit(&self, name: &str) -> String {
        let refs = &self.manifest().refs;
        let commit = refs.get(name);
        commit
            .unwrap_or_else(|| panic!("the fixture has no ref {name}"))
            .clone()
    }

    /// The repository's commits, parents first.
    fn commits(&self) -> &[ManifestCommit] {
        &self.manifest().commits
    }
}

/// `git import <source> <args…>` on `store`, as [`AGENT`].
fn import(store: &Store, source: &str, args: &[&str]) -> Run {
    let mut line = vec!["git", "import", source];
    line.extend_from_slice(args);
    store.run_as(AGENT, &line, &[])
}

/// A path in `store`'s temp dir where nothing is.
fn nowhere(store: &Store) -> String {
    let path = store.tmp().join("no-repository-here");
    path.to_str().expect("UTF-8 temp paths").to_owned()
}

/// An import into a store without `main` creates it at the tip's changeset and says what it
/// brought in: `linear`'s five commits, `main` moved, no old head, nothing lossy, no warning. The
/// history `log` lists is the git history, newest first; the head's `show` holds the commit's id
/// in `git.oid` and its git author and committer in `signatures`, and attributes it to no agent
/// (`unknown/unknown`: the M3 plan's decision 15). The store verifies clean.
#[test]
fn git_import_creates_the_target_and_reports_counts() {
    let store = Store::init();
    let linear = Source::new("linear");
    let tip = linear.commit("refs/heads/main");
    let result = import(&store, &linear.path, &[]).ok();
    assert_eq!(result["imported"], 5, "{result}");
    assert_eq!(result["skipped"], 0, "{result}");
    assert_eq!(result["ref_moved"], true, "{result}");
    assert_eq!(result["target_ref"], "main", "{result}");
    assert_eq!(result["git_head_sha"], tip.as_str(), "{result}");
    assert!(result.get("old_head").is_none(), "{result}");
    assert_eq!(result["lossy"], json!([]), "{result}");
    assert_eq!(result["lossy_total"], 0, "{result}");
    assert!(result.get("warnings").is_none(), "{result}");
    let head = result["head_changeset"].as_str().expect("an id").to_owned();

    let log = store.read(&["log"]).ok();
    let titles: Vec<&str> = (log["entries"].as_array().expect("a page").iter())
        .map(|entry| entry["title"].as_str().expect("a title"))
        .collect();
    assert_eq!(
        titles,
        [
            "add an empty file and a binary file",
            "add a/b/c.txt",
            "delete goodbye.txt",
            "modify hello.txt",
            "add hello.txt and goodbye.txt",
        ],
        "{log}"
    );
    assert_eq!(log["entries"][0]["id"], head.as_str(), "{log}");

    let show = store.show("main");
    assert_eq!(show["id"], head.as_str(), "{show}");
    assert_eq!(show["metadata"], json!({"git.oid": tip}), "{show}");
    let author = &show["signatures"]["author"];
    assert_eq!(author["name"], "Fixture Author", "{show}");
    assert_eq!(author["email"], "author@fixture.invalid", "{show}");
    let committer = &show["signatures"]["committer"];
    assert_eq!(committer["name"], "Fixture Committer", "{show}");
    assert_eq!(committer["email"], "committer@fixture.invalid", "{show}");
    assert_eq!(show["author"]["agent"], "unknown/unknown", "{show}");
    store.assert_clean("after an import");
}

/// The same import again finds the target at the tip's changeset: it imports nothing, skips the
/// one commit its walk stopped at, moves nothing, and writes nothing, neither a journal frame nor
/// an object.
#[test]
fn git_import_again_is_a_no_op() {
    let store = Store::init();
    let linear = Source::new("linear");
    let first = import(&store, &linear.path, &[]).ok();
    let (journal, objects) = (store.journal_len(), store.object_files());
    let again = import(&store, &linear.path, &[]).ok();
    assert_eq!(again["imported"], 0, "{again}");
    assert_eq!(again["skipped"], 1, "{again}");
    assert_eq!(again["ref_moved"], false, "{again}");
    assert_eq!(again["head_changeset"], first["head_changeset"], "{again}");
    assert_eq!(again["old_head"], first["head_changeset"], "{again}");
    assert_eq!(store.journal_len(), journal, "the journal grew");
    assert_eq!(store.object_files(), objects, "objects were written");
}

/// A submodule's entry is refused by default: `E_GIT_IMPORT_LOSSY` (exit 1, not retryable),
/// naming the commit, the class and the path, with a hint that names the flag allowing it; the
/// import stops before the frame that would hold that commit, so nothing is written and no ref
/// is made.
#[test]
fn git_import_of_a_gitlink_is_e_git_import_lossy_naming_it() {
    let store = Store::init();
    let gitlinks = Source::new("gitlinks");
    let journal = store.journal_len();
    let failed = import(&store, &gitlinks.path, &[]);
    assert_eq!(failed.exit(), 1, "{failed}");
    let error = failed.err("E_GIT_IMPORT_LOSSY");
    let second = &gitlinks.commits()[1].oid;
    assert_eq!(
        error["details"],
        json!({"commit": second, "class": "gitlinks", "path": "lib/sub"}),
        "{failed}"
    );
    assert_eq!(error["retryable"], false, "{failed}");
    let hint = error["hint"].as_str().unwrap_or_default();
    assert!(hint.contains("--lossy gitlinks"), "{hint}");
    assert_eq!(store.journal_len(), journal, "nothing is written");
    let refs = store.read(&["ref", "list"]).ok();
    assert_eq!(refs["refs"], json!([]), "{refs}");
}

/// `--gitlinks drop` is `--lossy gitlinks`, and `--lossy all` allows gitlinks too: the three
/// imports make the same changesets. The gitlinks are dropped, and `only/`, which held nothing
/// else, with them; the two commits that held one are listed, and each changeset names the
/// class in `git.lossy`. `--gitlinks refuse`, given, decides the class whatever `--lossy` says.
#[test]
fn git_import_with_gitlinks_dropped_marks_git_lossy() {
    let gitlinks = Source::new("gitlinks");
    let commits = gitlinks.commits();
    let (dropped, allowed, all) = (Store::init(), Store::init(), Store::init());
    let by_drop = import(&dropped, &gitlinks.path, &["--gitlinks", "drop"]).ok();
    let by_lossy = import(&allowed, &gitlinks.path, &["--lossy", "gitlinks"]).ok();
    let by_all = import(&all, &gitlinks.path, &["--lossy", "all"]).ok();
    assert_eq!(by_drop, by_lossy, "--gitlinks drop is --lossy gitlinks");
    assert_eq!(by_drop, by_all, "--lossy all allows gitlinks");
    assert_eq!(by_drop["imported"], 3, "{by_drop}");
    assert_eq!(by_drop["lossy_total"], 2, "{by_drop}");
    let listed = by_drop["lossy"].as_array().expect("a list");
    assert_eq!(listed.len(), 2, "{by_drop}");
    for (entry, commit) in listed.iter().zip(&commits[1..]) {
        assert_eq!(entry["commit"], commit.oid.as_str(), "{by_drop}");
        assert_eq!(entry["classes"], json!(["gitlinks"]), "{by_drop}");
    }
    assert_eq!(
        listed[1]["changeset"], by_drop["head_changeset"],
        "{by_drop}"
    );

    let show = dropped.show("main");
    let metadata = json!({"git.lossy": "gitlinks", "git.oid": commits[2].oid});
    assert_eq!(show["metadata"], metadata, "{show}");
    let root = dropped.read(&["ls", "main"]).ok();
    let names: Vec<&str> = (root["entries"].as_array().expect("a listing").iter())
        .map(|entry| entry["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(names, ["lib"], "{root}");
    let lib = dropped.read(&["ls", "main:lib"]).ok();
    assert_eq!(lib["entries"][0]["name"], "readme.txt", "{lib}");
    assert_eq!(lib["entries"].as_array().map(Vec::len), Some(1), "{lib}");
    dropped.assert_clean("after a lossy import");

    let refused = import(
        &Store::init(),
        &gitlinks.path,
        &["--lossy", "all", "--gitlinks", "refuse"],
    );
    refused.err("E_GIT_IMPORT_LOSSY");
}

/// A source rewritten since the import the target holds is a divergence: `refs`' `older`
/// imported, then `rewritten`, which leaves `older`'s tip out, is `E_GIT_IMPORT_DIVERGED`, exit
/// 3 (a conflict), naming the target, its head, the git commit that head holds and the tip; it
/// writes nothing, and the target stays.
#[test]
fn git_import_diverged_exits_3() {
    let store = Store::init();
    let refs = Source::new("refs");
    let older = import(&store, &refs.path, &["--ref", "older"]).ok();
    assert_eq!(older["imported"], 3, "{older}");
    let journal = store.journal_len();
    let failed = import(&store, &refs.path, &["--ref", "rewritten"]);
    assert_eq!(failed.exit(), 3, "{failed}");
    let error = failed.err("E_GIT_IMPORT_DIVERGED");
    assert_eq!(error["retryable"], false, "{failed}");
    let details = json!({
        "target_ref": "main",
        "target_head": older["head_changeset"],
        "base": refs.commit("refs/heads/older"),
        "git_head": refs.commit("refs/heads/rewritten"),
    });
    assert_eq!(error["details"], details, "{failed}");
    assert_eq!(store.journal_len(), journal, "nothing is written");
    assert_eq!(store.show("main")["id"], older["head_changeset"]);
}

/// A path where no git repository is, missing or a plain directory, is `E_GIT_NOT_A_REPO`, exit
/// 6, naming the path as it was given.
#[test]
fn git_import_of_a_missing_path_is_e_git_not_a_repo_exit_6() {
    let store = Store::init();
    let missing = nowhere(&store);
    let plain = store.tmp().to_str().expect("UTF-8 temp paths").to_owned();
    for path in [missing, plain] {
        let failed = import(&store, &path, &[]);
        assert_eq!(failed.exit(), 6, "{failed}");
        let error = failed.err("E_GIT_NOT_A_REPO");
        assert_eq!(error["details"], json!({"git_path": path}), "{failed}");
    }
}

/// A `--ref` that names nothing in the source is `E_GIT_REF_NOT_FOUND`, exit 6, naming it; one
/// that names an object other than a commit says which kind it named.
#[test]
fn git_import_of_an_unknown_ref_is_e_git_ref_not_found_exit_6() {
    let store = Store::init();
    let linear = Source::new("linear");
    let failed = import(&store, &linear.path, &["--ref", "no-such-branch"]);
    assert_eq!(failed.exit(), 6, "{failed}");
    let error = failed.err("E_GIT_REF_NOT_FOUND");
    assert_eq!(
        error["details"],
        json!({"git_ref": "no-such-branch"}),
        "{failed}"
    );

    let tree = &linear.commits()[0].tree;
    let failed = import(&store, &linear.path, &["--ref", tree]);
    assert_eq!(failed.exit(), 6, "{failed}");
    let error = failed.err("E_GIT_REF_NOT_FOUND");
    let details = json!({"git_ref": tree, "kind": "tree"});
    assert_eq!(error["details"], details, "{failed}");
}

/// A repository the import does not read as it is fails before anything is read past its
/// configuration: a shallow clone is `E_GIT_UNSUPPORTED` with `details.reason` `shallow` and a
/// hint to deepen it; a SHA-256 repository, with `reason` `object_format` and the format.
#[test]
fn git_import_of_a_shallow_clone_is_e_git_unsupported() {
    let store = Store::init();
    let shallow = Source::new("shallow");
    let failed = import(&store, &shallow.path, &[]);
    assert_eq!(failed.exit(), 1, "{failed}");
    let error = failed.err("E_GIT_UNSUPPORTED");
    assert_eq!(error["details"], json!({"reason": "shallow"}), "{failed}");
    let hint = error["hint"].as_str().unwrap_or_default();
    assert!(hint.contains("fetch --unshallow"), "{hint}");

    let sha256 = Source::new("sha256");
    let failed = import(&store, &sha256.path, &[]);
    let error = failed.err("E_GIT_UNSUPPORTED");
    let details = json!({"reason": "object_format", "object_format": "sha256"});
    assert_eq!(error["details"], details, "{failed}");
    let refs = store.read(&["ref", "list"]).ok();
    assert_eq!(refs["refs"], json!([]), "{refs}");
}

/// An import is a mutation, so it needs the caller's identity: without one it is
/// `E_IDENTITY_REQUIRED`, exit 5, before the source is opened (a missing source says nothing of
/// itself).
#[test]
fn git_import_needs_an_identity() {
    let store = Store::init();
    let missing = nowhere(&store);
    let failed = store.read(&["git", "import", &missing]);
    assert_eq!(failed.exit(), 5, "{failed}");
    failed.err("E_IDENTITY_REQUIRED");
}

/// A target that is no ref name is `E_REF_NAME_INVALID`, naming it, before the source is opened:
/// nothing is written, and a missing source says nothing of itself.
#[test]
fn git_import_into_a_bad_ref_name_is_e_ref_name_invalid() {
    let store = Store::init();
    let linear = Source::new("linear");
    let journal = store.journal_len();
    for source in [linear.path.clone(), nowhere(&store)] {
        let failed = import(&store, &source, &["--into", "feature/x"]);
        assert_eq!(failed.exit(), 1, "{failed}");
        let error = failed.err("E_REF_NAME_INVALID");
        assert_eq!(error["details"], json!({"name": "feature/x"}), "{failed}");
    }
    assert_eq!(store.journal_len(), journal, "nothing is written");
}

/// `swarmops mcp` serves no `git_import` tool: `tools/list` names none, and a `tools/call` of it
/// is refused as a call of an unknown tool (`-32602`), so that no agent of the server can make it
/// read a path. (`mcp_tools_list_schema_snapshot` pins the tools themselves.)
#[test]
fn git_import_is_not_an_mcp_tool() {
    let store = Store::init();
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": {"name": "swarmops-tests", "version": "0"},
    });
    let list = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list",
        "params": {"_meta": meta},
    });
    let call = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {"name": "git_import", "arguments": {"git_path": "."}, "_meta": meta},
    });
    let args = ["--repo", store.path(), "--as", AGENT, "mcp"];
    let served = run_bin(BIN, &args, &[], &format!("{list}\n{call}\n"));
    assert_eq!(served.exit(), 0, "{served}");
    let replies: Vec<Value> = (served.stdout.lines())
        .map(|line| serde_json::from_str(line).expect("one JSON-RPC message a line"))
        .collect();
    let [listed, called] = &replies[..] else {
        panic!("two replies: {served}");
    };
    let names: Vec<&str> = (listed["result"]["tools"].as_array().expect("tools").iter())
        .map(|tool| tool["name"].as_str().expect("a name"))
        .collect();
    assert!(names.contains(&"commit"), "{listed}");
    assert!(!names.contains(&"git_import"), "{listed}");
    assert_eq!(called["id"], 2, "{called}");
    assert_eq!(called["error"]["code"], -32602, "{called}");
}

/// `git export <destination> <args…>` on `store`, with no identity: an export needs none (the M8
/// plan's decision 8).
fn export(store: &Store, destination: &str, args: &[&str]) -> Run {
    let mut line = vec!["git", "export", destination];
    line.extend_from_slice(args);
    store.read(&line)
}

/// `path` as the command line gives it.
fn utf8(path: &std::path::Path) -> String {
    path.to_str().expect("UTF-8 temp paths").to_owned()
}

/// The loose object files of the git repository whose git directory is `git_dir`: those of its
/// fan-out directories, named by two hex digits (`info/` and `pack/` are not).
fn loose_objects(git_dir: &std::path::Path) -> usize {
    let objects = std::fs::read_dir(git_dir.join("objects")).expect("objects/");
    let mut count = 0;
    for fanout in objects {
        let fanout = fanout.expect("an entry of objects/");
        if fanout.file_name().len() == 2 {
            count += std::fs::read_dir(fanout.path()).expect("a fan-out").count();
        }
    }
    count
}

/// The commit the branch `name` of the git repository at `git_dir` names, read through the git
/// side's reader.
fn branch_at(git_dir: &std::path::Path, name: &str) -> String {
    use swarmops_git::{GitSource as _, GixSource};

    let repository = GixSource::open(git_dir).expect("open the repository");
    let commit = repository.resolve(&format!("refs/heads/{name}"));
    commit.expect("the branch exists").to_string()
}

/// A store that imported the fixture `name` from its `HEAD`; the fixture, and the head's
/// changeset.
fn imported(name: &str) -> (Store, Source, String) {
    let store = Store::init();
    let source = Source::new(name);
    let result = import(&store, &source.path, &[]).ok();
    let head = result["head_changeset"].as_str().expect("an id").to_owned();
    (store, source, head)
}

/// `linear` imported and exported into an empty repository, whose branch git then moved to a
/// commit of another history (the second commit of `roots`, written there through the git
/// side's writer, its tree and parent not): what the tests of a moved branch start from.
struct Moved {
    /// The store, which imported `linear`.
    store: Store,
    /// The repository the store exported to, and git moved the branch of.
    destination: swarmops_testkit::git::EmptyRepository,
    /// The destination's path.
    path: String,
    /// The changeset at `main`'s head, `linear`'s tip imported.
    head: String,
    /// `linear`'s tip: the export of `head`.
    tip: String,
    /// The commit of `roots` the branch names now.
    foreign: String,
}

impl Moved {
    /// The store, the destination and the branch as the tests of a moved branch start from them.
    fn new() -> Moved {
        use swarmops_git::{BranchUpdate, GitOid, GitSink as _, GixSink, RawKind};

        let (store, linear, head) = imported("linear");
        let tip = linear.commit("refs/heads/main");
        let destination = swarmops_testkit::git::empty_repository();
        let path = utf8(&destination.dir);
        assert_eq!(export(&store, &path, &[]).ok()["head_sha"], tip.as_str());
        let roots = Source::new("roots");
        let commit = &roots.commits()[1];
        let mut sink = GixSink::open(&destination.dir).expect("open the destination");
        let foreign = sink.write_object(RawKind::Commit, &commit.raw).unwrap();
        assert_eq!(foreign.to_string(), commit.oid);
        sink.sync(&[foreign]).unwrap();
        let exported: GitOid = tip.parse().expect("a commit id");
        let moved = sink.cas_branch("main", Some(exported), foreign);
        assert_eq!(moved, Ok(BranchUpdate::Applied));
        Moved {
            store,
            destination,
            path,
            head,
            tip,
            foreign: foreign.to_string(),
        }
    }
}

/// A repository with a work tree, whose `HEAD` names `main`: an empty repository's files as its
/// `.git`, `bare = false`.
fn work_tree() -> tempfile::TempDir {
    let made = swarmops_testkit::git::empty_repository();
    let work = tempfile::tempdir().expect("make a temp dir");
    let dot_git = work.path().join(".git");
    std::fs::rename(&made.dir, &dot_git).expect("move the repository under .git");
    let config = "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n";
    std::fs::write(dot_git.join("config"), config).expect("write .git/config");
    work
}

/// `linear` imported, then exported into an empty repository: exit 0, its five commits written
/// with every tree and blob they reach, and the branch created at the fixture's tip; no old sha,
/// nothing incremental, nothing rewritten, and `main`, read once, echoed in `meta.resolved`. Read
/// back through the git side, every commit of the fixture is there with its own bytes, every tree
/// with its bytes and every blob with its size.
#[test]
fn git_export_round_trips_an_imported_fixture() {
    use swarmops_git::{GitOid, GitSource as _, GixSource, RawKind};

    let (store, linear, head) = imported("linear");
    let tip = linear.commit("refs/heads/main");
    let destination = swarmops_testkit::git::empty_repository();
    let run = export(&store, &utf8(&destination.dir), &[]);
    let result = run.ok();
    let manifest = linear.manifest();
    let objects = manifest.commits.len() + manifest.trees.len() + manifest.blobs.len();
    assert_eq!(result["exported"], 5, "{result}");
    assert_eq!(result["objects_written"], objects, "{result}");
    assert_eq!(result["head_changeset"], head.as_str(), "{result}");
    assert_eq!(result["head_sha"], tip.as_str(), "{result}");
    assert_eq!(result["branch"], "main", "{result}");
    assert!(result.get("old_sha").is_none(), "{result}");
    assert_eq!(result["branch_moved"], true, "{result}");
    assert_eq!(result["incremental"], false, "{result}");
    assert_eq!(result["rewritten"], 0, "{result}");
    assert!(result.get("warnings").is_none(), "{result}");
    let resolved = &run.envelope()["meta"]["resolved"];
    assert_eq!(resolved, &json!({"main": head}), "{run}");

    let repository = GixSource::open(&destination.dir).expect("open the export");
    assert_eq!(branch_at(&destination.dir, "main"), tip);
    let read = |oid: &str| {
        let oid: GitOid = oid.parse().expect("an object id");
        repository.read(oid).expect("the export holds it")
    };
    for commit in linear.commits() {
        assert_eq!(read(&commit.oid), (RawKind::Commit, commit.raw.clone()));
    }
    for (oid, bytes) in &manifest.trees {
        assert_eq!(read(oid), (RawKind::Tree, bytes.clone()), "tree {oid}");
    }
    for (oid, size) in &manifest.blobs {
        let (kind, bytes) = read(oid);
        let read = (kind, u64::try_from(bytes.len()).expect("a length"));
        assert_eq!(read, (RawKind::Blob, *size), "blob {oid}");
    }
}

/// The same export again finds the branch at the head's export: it exports nothing, writes no
/// object, leaves the branch where it is (`old_sha` is `head_sha`) and goes on from the export the
/// branch holds; `--human` says it in a line.
#[test]
fn git_export_again_is_a_no_op() {
    let (store, linear, _) = imported("linear");
    let tip = linear.commit("refs/heads/main");
    let destination = swarmops_testkit::git::empty_repository();
    let path = utf8(&destination.dir);
    let first = export(&store, &path, &[]).ok();
    let files = loose_objects(&destination.dir);
    let again = export(&store, &path, &[]).ok();
    assert_eq!(again["exported"], 0, "{again}");
    assert_eq!(again["objects_written"], 0, "{again}");
    assert_eq!(again["branch_moved"], false, "{again}");
    assert_eq!(again["incremental"], true, "{again}");
    assert_eq!(again["head_sha"], first["head_sha"], "{again}");
    assert_eq!(again["old_sha"], tip.as_str(), "{again}");
    assert_eq!(
        loose_objects(&destination.dir),
        files,
        "objects were written"
    );

    let human = store.read(&["--human", "git", "export", &path]);
    assert_eq!(human.exit(), 0, "{human}");
    let line = format!("main is up to date at {tip}");
    assert_eq!(human.stdout.lines().next(), Some(line.as_str()), "{human}");
}

/// A branch moved in git to a commit of another history after an export: the next export is
/// `E_GIT_EXPORT_DIVERGED`, exit 3, not retryable, `details.reason` `branch_moved`, naming the
/// branch, the commit it names and the source's head, with a hint that names `--force`; nothing
/// is written, and the branch stays where git moved it.
#[test]
fn git_export_of_a_moved_branch_exits_3() {
    let moved = Moved::new();
    let files = loose_objects(&moved.destination.dir);
    let failed = export(&moved.store, &moved.path, &[]);
    assert_eq!(failed.exit(), 3, "{failed}");
    let error = failed.err("E_GIT_EXPORT_DIVERGED");
    assert_eq!(error["retryable"], false, "{failed}");
    let details = json!({
        "reason": "branch_moved",
        "branch": "main",
        "git_head": moved.foreign,
        "head_changeset": moved.head,
    });
    assert_eq!(error["details"], details, "{failed}");
    let hint = error["hint"].as_str().unwrap_or_default();
    assert!(hint.contains("--force"), "{hint}");
    assert_eq!(
        loose_objects(&moved.destination.dir),
        files,
        "nothing is written"
    );
    assert_eq!(branch_at(&moved.destination.dir, "main"), moved.foreign);
}

/// `--force` moves a moved branch all the same, by compare-and-swap against the commit the export
/// read: to the export of the source's head again, from the commit git moved it to (`old_sha`),
/// not incrementally, with a warning that says it had diverged. The first export's objects are
/// still there, so it writes no commit: it reads them back.
#[test]
fn git_export_force_moves_it() {
    let moved = Moved::new();
    let forced = export(&moved.store, &moved.path, &["--force"]).ok();
    assert_eq!(forced["branch_moved"], true, "{forced}");
    assert_eq!(forced["old_sha"], moved.foreign.as_str(), "{forced}");
    assert_eq!(forced["head_sha"], moved.tip.as_str(), "{forced}");
    assert_eq!(forced["incremental"], false, "{forced}");
    assert_eq!(forced["exported"], 0, "{forced}");
    let warnings = forced["warnings"].as_array().expect("a warning");
    assert_eq!(warnings.len(), 1, "{forced}");
    let warning = warnings[0].as_str().unwrap_or_default();
    assert!(warning.contains("branch_moved"), "{warning}");
    assert_eq!(branch_at(&moved.destination.dir, "main"), moved.tip);
}

/// In a repository with a work tree whose `HEAD` names the branch, the export refuses to move it,
/// as `git push` does by default: `E_INVALID_ARG`, exit 2, `details.reason` `checked_out` and the
/// branch, `--force` or not, and nothing is written. Another branch of the same repository takes
/// the export.
#[test]
fn git_export_into_a_checked_out_branch_is_refused() {
    let (store, _, _) = imported("linear");
    let work = work_tree();
    let path = utf8(work.path());
    for args in [&[][..], &["--force"]] {
        let failed = export(&store, &path, args);
        assert_eq!(failed.exit(), 2, "{failed}");
        let error = failed.err("E_INVALID_ARG");
        let details = json!({"reason": "checked_out", "branch": "main"});
        assert_eq!(error["details"], details, "{failed}");
    }
    assert_eq!(
        loose_objects(&work.path().join(".git")),
        0,
        "nothing is written"
    );
    let other = export(&store, &path, &["--branch", "exported"]).ok();
    assert_eq!(other["exported"], 5, "{other}");
    assert_eq!(other["branch"], "exported", "{other}");
}

/// A path where no git repository is, missing or a plain directory, is `E_GIT_NOT_A_REPO`, exit
/// 6, naming the path as it was given, with the hint that makes one: an export writes into a
/// repository that exists.
#[test]
fn git_export_of_a_missing_path_is_e_git_not_a_repo_exit_6() {
    let (store, _, _) = imported("linear");
    let missing = nowhere(&store);
    let plain = utf8(store.tmp());
    for path in [missing, plain] {
        let failed = export(&store, &path, &[]);
        assert_eq!(failed.exit(), 6, "{failed}");
        let error = failed.err("E_GIT_NOT_A_REPO");
        assert_eq!(error["details"], json!({"git_path": path}), "{failed}");
        let hint = error["hint"].as_str().unwrap_or_default();
        assert!(hint.contains("git init --bare"), "{hint}");
    }
}

/// A source ref the store does not have is `E_REV_NOT_FOUND`, exit 6, naming it, with the hint
/// to list the refs: `main` in a store that has none yet, and a release ref no one made. Nothing
/// is written.
#[test]
fn git_export_of_an_unknown_source_ref_is_e_rev_not_found() {
    let store = Store::init();
    let destination = swarmops_testkit::git::empty_repository();
    let path = utf8(&destination.dir);
    for (args, rev) in [
        (&[][..], "main"),
        (&["--ref", "release/none"], "release/none"),
    ] {
        let failed = export(&store, &path, args);
        assert_eq!(failed.exit(), 6, "{failed}");
        let error = failed.err("E_REV_NOT_FOUND");
        assert_eq!(error["details"], json!({"rev": rev}), "{failed}");
        assert_eq!(error["hint"], "swarmops ref list", "{failed}");
    }
    assert_eq!(loose_objects(&destination.dir), 0, "nothing is written");
}

/// A branch name git does not take is `E_REF_NAME_INVALID`, naming it, with a hint that says how
/// git checks one, and nothing is written. A source ref that is no store ref name is the same
/// code, before the repository is opened, so that a missing one says nothing of itself.
#[test]
fn git_export_into_a_bad_branch_name_is_e_ref_name_invalid() {
    let (store, _, _) = imported("linear");
    let destination = swarmops_testkit::git::empty_repository();
    let path = utf8(&destination.dir);
    for name in ["a..b", "x.lock", ""] {
        let failed = export(&store, &path, &["--branch", name]);
        assert_eq!(failed.exit(), 1, "{failed}");
        let error = failed.err("E_REF_NAME_INVALID");
        assert_eq!(error["details"], json!({"name": name}), "{failed}");
        let hint = error["hint"].as_str().unwrap_or_default();
        assert!(hint.contains("git check-ref-format --branch"), "{hint}");
    }
    assert_eq!(loose_objects(&destination.dir), 0, "nothing is written");
    for repository in [path, nowhere(&store)] {
        let failed = export(&store, &repository, &["--ref", "feature/x"]);
        assert_eq!(failed.exit(), 1, "{failed}");
        let error = failed.err("E_REF_NAME_INVALID");
        assert_eq!(error["details"], json!({"name": "feature/x"}), "{failed}");
    }
}

/// A branch whose lock file is there, as another git process holding the branch or one that died
/// leaves it, is `E_GIT_WRITE`, exit 1, `details.reason` `ref_locked` with the lock's path in the
/// git directory, and a hint to remove it once no git process runs; the branch is not made. Once
/// the lock is gone, the same export makes it, at the export of the head.
#[test]
fn git_export_with_a_held_ref_lock_is_e_git_write() {
    let (store, linear, _) = imported("linear");
    let destination = swarmops_testkit::git::empty_repository();
    let path = utf8(&destination.dir);
    let lock = destination.dir.join("refs/heads/main.lock");
    std::fs::write(&lock, "").expect("write the lock");
    let failed = export(&store, &path, &[]);
    assert_eq!(failed.exit(), 1, "{failed}");
    let error = failed.err("E_GIT_WRITE");
    let details = json!({"reason": "ref_locked", "path": "refs/heads/main.lock"});
    assert_eq!(error["details"], details, "{failed}");
    assert_eq!(error["retryable"], false, "{failed}");
    let hint = error["hint"].as_str().unwrap_or_default();
    assert!(hint.contains("refs/heads/main.lock"), "{hint}");
    assert!(
        !destination.dir.join("refs/heads/main").exists(),
        "no branch"
    );

    std::fs::remove_file(&lock).expect("remove the lock");
    let again = export(&store, &path, &[]).ok();
    assert_eq!(again["branch_moved"], true, "{again}");
    let tip = linear.commit("refs/heads/main");
    assert_eq!(branch_at(&destination.dir, "main"), tip);
}

/// An export writes a git repository and the store's caches, never a journal frame, so it needs
/// no identity (the M8 plan's decision 8): without `--as` or `SWARMOPS_AGENT` it exits 0, and the
/// store's journal does not grow.
#[test]
fn git_export_needs_no_identity() {
    let (store, _, _) = imported("linear");
    let destination = swarmops_testkit::git::empty_repository();
    let journal = store.journal_len();
    let run = store.read(&["git", "export", &utf8(&destination.dir)]);
    assert_eq!(run.exit(), 0, "{run}");
    assert_eq!(run.ok()["exported"], 5, "{run}");
    assert_eq!(
        store.journal_len(),
        journal,
        "an export writes no journal frame"
    );
}

/// `swarmops mcp` serves no `git_export` tool: `tools/list` names none, and a `tools/call` of it
/// is refused as a call of an unknown tool (`-32602`), so that no agent of the server can make it
/// write a path. (`mcp_tools_list_schema_snapshot` pins the tools themselves.)
#[test]
fn git_export_is_not_an_mcp_tool() {
    let store = Store::init();
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": {"name": "swarmops-tests", "version": "0"},
    });
    let list = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list",
        "params": {"_meta": meta},
    });
    let call = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {"name": "git_export", "arguments": {"git_path": "."}, "_meta": meta},
    });
    let args = ["--repo", store.path(), "--as", AGENT, "mcp"];
    let served = run_bin(BIN, &args, &[], &format!("{list}\n{call}\n"));
    assert_eq!(served.exit(), 0, "{served}");
    let replies: Vec<Value> = (served.stdout.lines())
        .map(|line| serde_json::from_str(line).expect("one JSON-RPC message a line"))
        .collect();
    let [listed, called] = &replies[..] else {
        panic!("two replies: {served}");
    };
    let names: Vec<&str> = (listed["result"]["tools"].as_array().expect("tools").iter())
        .map(|tool| tool["name"].as_str().expect("a name"))
        .collect();
    assert!(names.contains(&"commit"), "{listed}");
    assert!(!names.contains(&"git_export"), "{listed}");
    assert_eq!(called["id"], 2, "{called}");
    assert_eq!(called["error"]["code"], -32602, "{called}");
}
