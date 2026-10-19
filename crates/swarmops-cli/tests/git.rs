//! `swarmops git import`, through the binary: a git repository's history brought into a store from
//! the test kit's git fixtures (`swarmops_testkit::git`), which real git made and which no test
//! runs git to read; the error each refusal exits with, and what it names; and that the import is
//! no MCP tool, since it reads its source on the caller's host (the M3 plan's decision 7).

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
