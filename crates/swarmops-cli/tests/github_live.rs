//! The live run against GitHub (M13), only when `SWARMOPS_GITHUB_LIVE=1`; every other test is
//! offline. Written by the M13 plan's task C12.
//!
//! The run reads three variables: `SWARMOPS_GITHUB_LIVE` (`1` turns it on),
//! `SWARMOPS_GITHUB_REPO` (the scratch repository, `owner/name`) and `SWARMOPS_GITHUB_TOKEN_ENV`,
//! the name of the variable that holds the token (default `GH_TOKEN`). The token reaches the
//! binary and `gh` through their environment only, never an argument. SwarmOps calls no GitHub
//! API: `gh` opens and merges the pull requests and reads the workflow runs.
//!
//! ```text
//!   so clone ─ land ─ so push main:so-live/<run> ─ the workflow on that sha succeeds
//!   for merge, squash, rebase:
//!       land ×2 ─ so push main:so-live/<run>-<method> ─ gh pr create ─ gh pr merge --<method>
//!       ─ so sync origin --no-push ─ main at GitHub's head, the lands dropped as 6.3 says
//!   land ─ so sync origin (pushes to the unprotected default branch)
//!   a second clone; a land in each ─ both so push main:so-live/<run>-race at once
//!       ─ one created, the other E_REMOTE_DIVERGED remote_moved
//!   every so-live branch deleted, never the default branch
//! ```

use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::support::{Racers, Run, Store};

/// The variable that turns the live run on, when it is `1`.
const LIVE_VAR: &str = "SWARMOPS_GITHUB_LIVE";

/// The variable that names the scratch repository, `owner/name`.
const REPO_VAR: &str = "SWARMOPS_GITHUB_REPO";

/// The variable that names the token's variable.
const TOKEN_ENV_VAR: &str = "SWARMOPS_GITHUB_TOKEN_ENV";

/// The token's variable when [`TOKEN_ENV_VAR`] is unset: the one `gh` reads.
const DEFAULT_TOKEN_ENV: &str = "GH_TOKEN";

/// The prefix of every branch the run pushes, and the only branches it deletes.
const BRANCH_PREFIX: &str = "so-live/";

/// The agent that commits and lands.
const AGENT: &str = "agent/live-1";

/// The remote's name in the store.
const ORIGIN: &str = "origin";

/// How long the workflow run for the pushed sha may take to finish.
const WORKFLOW_WAIT: Duration = Duration::from_mins(15);

/// How often the workflow runs are read.
const WORKFLOW_POLL: Duration = Duration::from_secs(15);

/// How many times a pull request's merge is tried while GitHub computes its mergeability.
const MERGE_TRIES: usize = 6;

/// How long between two tries of a merge.
const MERGE_PAUSE: Duration = Duration::from_secs(10);

/// How many changes each pull request carries.
const LANDS_PER_PULL: usize = 2;

/// A push's status when it made the branch.
const CREATED: &str = "created";

/// The error of a push whose lease failed.
const DIVERGED: &str = "E_REMOTE_DIVERGED";

/// Its reason when the branch moved since the last fetch: a lost race.
const REMOTE_MOVED: &str = "remote_moved";

/// The live run's settings, read from the environment. No `Debug`: it holds the token.
struct Live {
    /// The scratch repository, `owner/name`.
    repo: String,
    /// The token's variable, by its name.
    token_env: String,
    /// The token.
    token: String,
}

/// Whether the live run runs.
enum Gate {
    /// Off, and why.
    Off(String),
    /// On, with its settings.
    On(Live),
}

/// The gate: off unless `SWARMOPS_GITHUB_LIVE=1`; on, the repository and the token must be
/// there (an error names what is missing, never the token). `var` reads a variable.
fn live_enabled(var: &dyn Fn(&str) -> Option<String>) -> Result<Gate, String> {
    // 1. Off unless asked for.
    if var(LIVE_VAR).as_deref() != Some("1") {
        return Ok(Gate::Off(format!(
            "{LIVE_VAR} is not 1: the live run is off"
        )));
    }

    // 2. The repository, `owner/name`.
    let repo = var(REPO_VAR).unwrap_or_default();
    let parts: Vec<&str> = repo.split('/').collect();
    if parts.len() != 2 || parts.iter().any(|part| part.is_empty()) {
        return Err(format!("{LIVE_VAR}=1 needs {REPO_VAR} as owner/name"));
    }

    // 3. The token, from the variable its name gives.
    let token_env = var(TOKEN_ENV_VAR).unwrap_or_else(|| DEFAULT_TOKEN_ENV.to_owned());
    let token = var(&token_env).unwrap_or_default();
    if token.is_empty() {
        return Err(format!("{LIVE_VAR}=1 needs a token in {token_env}"));
    }
    Ok(Gate::On(Live {
        repo,
        token_env,
        token,
    }))
}

/// The gate itself, offline: without `SWARMOPS_GITHUB_LIVE` the live run is off, and says why.
#[test]
fn live_run_is_off_without_the_variable() {
    let unset = |_: &str| None;
    let gate = live_enabled(&unset).expect("no misconfiguration");
    assert!(
        matches!(gate, Gate::Off(reason) if reason.contains("SWARMOPS_GITHUB_LIVE")),
        "the reason names the variable"
    );
}

/// On, the gate wants the repository and the token, and names what is missing without the
/// token's value; with both it is on, the token read from the variable its name gives.
#[test]
fn live_run_needs_its_repository_and_token() {
    let token = "not-a-real-token";
    let with = |pairs: &'static [(&'static str, &'static str)]| {
        move |name: &str| {
            let found = pairs.iter().find(|(key, _)| *key == name);
            found.map(|(_, value)| (*value).to_owned())
        }
    };

    // 1. No repository.
    let lookup = with(&[(LIVE_VAR, "1"), ("GH_TOKEN", "not-a-real-token")]);
    let missing = live_enabled(&lookup).err().expect("refused");
    assert!(
        missing.contains(REPO_VAR) && !missing.contains(token),
        "{missing}"
    );

    // 2. No token in the variable named.
    let lookup = with(&[
        (LIVE_VAR, "1"),
        (REPO_VAR, "owner/name"),
        (TOKEN_ENV_VAR, "LIVE_TOKEN"),
        ("GH_TOKEN", "not-a-real-token"),
    ]);
    let missing = live_enabled(&lookup).err().expect("refused");
    assert!(missing.contains("LIVE_TOKEN"), "{missing}");

    // 3. Both: on.
    let lookup = with(&[
        (LIVE_VAR, "1"),
        (REPO_VAR, "owner/name"),
        ("GH_TOKEN", "not-a-real-token"),
    ]);
    let Ok(Gate::On(live)) = live_enabled(&lookup) else {
        panic!("on");
    };
    assert_eq!(live.repo, "owner/name");
    assert_eq!(live.token_env, DEFAULT_TOKEN_ENV);
    assert_eq!(live.token, token);
}

/// The verdict of two pushes racing to create one branch, by their envelopes: the winner's
/// index when one made the branch and the other lost the race (`E_REMOTE_DIVERGED`
/// `remote_moved`). Else both envelopes: a refusal read otherwise holds the remote's wording,
/// which becomes one more needle of the git crate's classifier.
fn race_verdict(envelopes: &[Value]) -> Result<usize, String> {
    let created = |envelope: &Value| envelope["result"]["status"] == CREATED;
    let moved = |envelope: &Value| {
        envelope["error"]["code"] == DIVERGED
            && envelope["error"]["details"]["reason"] == REMOTE_MOVED
    };

    // 1. One winner, one loser, in either order.
    match envelopes {
        [won, lost] if created(won) && moved(lost) => return Ok(0),
        [lost, won] if moved(lost) && created(won) => return Ok(1),
        _ => {}
    }

    // 2. Anything else, printed whole.
    let said: Vec<String> = envelopes.iter().map(Value::to_string).collect();
    Err(format!(
        "a lost race must be {DIVERGED} {REMOTE_MOVED}; the pushes said:\n{}",
        said.join("\n")
    ))
}

/// A push's envelope: `created`, as the winner's.
fn created_envelope() -> Value {
    serde_json::json!({"api": 1, "ok": true, "result": {"status": CREATED, "new": "a1"}})
}

/// A push's envelope: an error of `code` for `reason`, with git's `stderr_tail`.
fn refused_envelope(code: &str, reason: &str, stderr_tail: &str) -> Value {
    let details =
        serde_json::json!({"remote": ORIGIN, "reason": reason, "stderr_tail": stderr_tail});
    serde_json::json!({"api": 1, "ok": false, "error": {"code": code, "details": details}})
}

/// Offline: one push created the branch and the other lost the race as `remote_moved`; the
/// winner is found in either order.
#[test]
fn a_lost_race_is_remote_moved() {
    let moved = refused_envelope(DIVERGED, REMOTE_MOVED, "");
    let first = [created_envelope(), moved.clone()];
    assert_eq!(race_verdict(&first), Ok(0));
    let second = [moved, created_envelope()];
    assert_eq!(race_verdict(&second), Ok(1));
}

/// Offline: a loser read otherwise fails the run, and the failure prints what the remote said
/// (GitHub's wording, the needle a fix of the classifier takes); two winners fail it too.
#[test]
fn another_refusal_prints_its_wording() {
    let said = " ! [remote rejected] main -> so-live/1-race (cannot lock ref)";
    let declined = refused_envelope("E_REMOTE_REJECTED", "declined", said);
    let verdict = race_verdict(&[created_envelope(), declined]);
    let printed = verdict.expect_err("declined is no lost race");
    assert!(printed.contains(said), "{printed}");
    assert!(printed.contains("E_REMOTE_REJECTED"), "{printed}");

    let both = race_verdict(&[created_envelope(), created_envelope()]);
    assert!(both.is_err(), "two winners: {both:?}");
}

/// The round trip with GitHub (roadmap Topology 2, criterion 4), on the scratch repository
/// `SWARMOPS_GITHUB_REPO` names.
#[test]
#[ignore = "live: GitHub over the network, only with SWARMOPS_GITHUB_LIVE=1"]
fn round_trip_with_github() {
    let lookup = |name: &str| std::env::var(name).ok();
    let settings = match live_enabled(&lookup) {
        Ok(Gate::On(settings)) => settings,
        Ok(Gate::Off(reason)) => {
            eprintln!("skipped: {reason}");
            return;
        }
        Err(wrong) => panic!("{wrong}"),
    };
    let github = GitHub::new(&settings);
    let run = run_id();
    let mut cleanup = Cleanup {
        github: &github,
        branches: Vec::new(),
    };

    // 1. The clone, over HTTPS with the token's variable.
    let home = tempfile::tempdir().expect("a temp HOME");
    let home_text = home.path().to_str().expect("UTF-8 temp paths");
    let env = [
        ("HOME", home_text),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_TERMINAL_PROMPT", "0"),
        (settings.token_env.as_str(), settings.token.as_str()),
    ];
    let store = Store::empty().with_env(&env);
    let url = format!("https://github.com/{}.git", settings.repo);
    let line = [
        "clone",
        url.as_str(),
        "--into",
        store.path(),
        "--token-env",
        settings.token_env.as_str(),
    ];
    let cloned = store.run_as(AGENT, &line, &[]).ok();
    let default = cloned["remote"]["branch"]
        .as_str()
        .expect("the followed branch")
        .to_owned();
    assert_eq!(main_at(&store), github.head_changeset(&store, &default));

    // 2. A land, pushed to a run branch; the repository's workflow passes on its sha.
    land(&store, &run, "push");
    let branch = format!("{BRANCH_PREFIX}{run}");
    let sha = push(&store, &mut cleanup, &branch);
    github.wait_for_workflow(&sha);

    // 3. Three pull requests, one per merge method, each followed by a sync that pushes nothing.
    for method in ["merge", "squash", "rebase"] {
        let lands: Vec<String> = (0..LANDS_PER_PULL)
            .map(|n| land(&store, &run, &format!("{method}-{n}")))
            .collect();
        let branch = format!("{BRANCH_PREFIX}{run}-{method}");
        push(&store, &mut cleanup, &branch);
        let merged = github.pull_and_merge(&branch, &default, method);
        eprintln!(
            "{method}: GitHub wrote {merged}: {:?}",
            github.message(&merged)
        );

        // The sync: main at GitHub's head, every land kept or dropped as §6.3 says.
        let line = ["sync", ORIGIN, "--no-push"];
        let synced = store.run_as(AGENT, &line, &[]).ok();
        assert_eq!(synced["remote_head"]["new"], merged.as_str(), "{synced}");
        let dropped = synced["dropped"].as_array().expect("dropped");
        let expected = if method == "merge" { 0 } else { lands.len() };
        assert_eq!(dropped.len(), expected, "{method}: {synced}");
        assert!(synced["published"].as_array().is_some_and(Vec::is_empty));
        assert_at_head(&store, &github, &default);
    }

    // 4. A sync that pushes, to the default branch, which has no protection.
    land(&store, &run, "sync");
    let synced = store.run_as(AGENT, &["sync", ORIGIN], &[]).ok();
    assert_eq!(synced["status"], "pushed", "{synced}");
    let pushed = synced["remote_head"]["new"].as_str().expect("a sha");
    assert_eq!(github.head(&default), pushed);
    assert_at_head(&store, &github, &default);

    // 5. Two stores create one branch at once: one wins, the other lost the race.
    race(&store, &env, &url, &settings, &mut cleanup, &run);
    store.assert_clean("after the live run");
}

/// A second store cloned from `url`; each of the two lands its own change, and both push
/// `main:so-live/<run>-race` at once, each with a lease on no branch (neither fetched it). The
/// loser must read `E_REMOTE_DIVERGED` `remote_moved`, whether git's lease saw the winner or
/// GitHub refused the second update; GitHub's branch is the winner's commit.
fn race(
    store: &Store,
    env: &[(&str, &str)],
    url: &str,
    settings: &Live,
    cleanup: &mut Cleanup<'_>,
    run: &str,
) {
    // 1. The second store, and a change on each.
    let other = Store::empty().with_env(env);
    let line = [
        "clone",
        url,
        "--into",
        other.path(),
        "--token-env",
        settings.token_env.as_str(),
    ];
    other.run_as(AGENT, &line, &[]).ok();
    land(store, run, "race-a");
    land(&other, run, "race-b");

    // 2. Both pushes, let go together.
    let branch = format!("{BRANCH_PREFIX}{run}-race");
    cleanup.branches.push(branch.clone());
    let target = format!("main:{branch}");
    let mut racers = Racers::new(store.tmp());
    for racer in [store, &other] {
        let line = ["--repo", racer.path(), "push", ORIGIN, target.as_str()];
        racers.spawn(&line, env);
    }
    let runs = racers.race();
    let envelopes: Vec<Value> = runs.iter().map(Run::envelope).collect();

    // 3. The verdict, and the winner's commit on GitHub.
    let won = race_verdict(&envelopes).unwrap_or_else(|said| panic!("{said}"));
    let new = envelopes[won]["result"]["new"].as_str().expect("a sha");
    assert_eq!(cleanup.github.head(&branch), new);
    other.assert_clean("after the race");
}

/// The run's id: the workflow's run, else this process's start.
fn run_id() -> String {
    if let Ok(id) = std::env::var("GITHUB_RUN_ID") {
        return id;
    }
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970");
    format!("local-{}-{}", since.as_secs(), std::process::id())
}

/// The changeset `main` names in `store`.
fn main_at(store: &Store) -> Option<String> {
    let refs = store.read(&["ref", "list"]).ok();
    let listed = refs["refs"].as_array().expect("refs");
    let main = listed.iter().find(|entry| entry["name"] == "main")?;
    Some(main["target"].as_str().expect("a target").to_owned())
}

/// An agent's change at `live/<run>/<name>.txt`, committed on `main` and landed with a key;
/// the changeset.
fn land(store: &Store, run: &str, name: &str) -> String {
    let head = main_at(store).expect("main");
    let path = format!("live/{run}/{name}.txt");
    let change = store.commit(AGENT, &path, &format!("{name}\n"), Some(&head));
    let key = format!("live-{run}-{name}");
    let line = ["land", change.as_str(), "--key", key.as_str()];
    store.run_as(AGENT, &line, &[]).ok();
    change
}

/// `so push origin main:<branch>`, which creates the branch and rewrites no commit; the sha
/// it names. The branch is deleted at the end of the run.
fn push(store: &Store, cleanup: &mut Cleanup<'_>, branch: &str) -> String {
    let target = format!("main:{branch}");
    let pushed = store.read(&["push", ORIGIN, target.as_str()]).ok();
    cleanup.branches.push(branch.to_owned());
    assert_eq!(pushed["status"], "created", "{pushed}");
    assert_eq!(pushed["rewritten"], 0, "{pushed}");
    pushed["new"].as_str().expect("a sha").to_owned()
}

/// `sync status` after a sync: `main` and GitHub's `branch` are the same commit, nothing ahead
/// or behind, and the store verifies clean.
fn assert_at_head(store: &Store, github: &GitHub, branch: &str) {
    let status = store.read(&["sync", "status", ORIGIN]).ok();
    assert_eq!(
        status["remote_head"]["sha"],
        github.head(branch).as_str(),
        "{status}"
    );
    assert_eq!(status["ahead"], 0, "{status}");
    assert_eq!(status["behind"], 0, "{status}");
    assert_eq!(
        main_at(store),
        status["remote_head"]["changeset"]
            .as_str()
            .map(str::to_owned)
    );
    store.assert_clean("after a sync");
}

/// The scratch repository through `gh`, with the token in its environment.
struct GitHub {
    /// The repository, `owner/name`.
    repo: String,
    /// The token, for `gh`'s `GH_TOKEN`.
    token: String,
    /// The directory `gh` runs in: outside any git repository, so that it takes none for the
    /// repository.
    dir: tempfile::TempDir,
}

impl GitHub {
    /// The repository `live` names.
    fn new(live: &Live) -> GitHub {
        GitHub {
            repo: live.repo.clone(),
            token: live.token.clone(),
            dir: tempfile::tempdir().expect("a temp dir"),
        }
    }

    /// `gh <args>`: its output, or its error.
    fn try_gh(&self, args: &[&str]) -> Result<String, String> {
        let output = Command::new("gh")
            .args(args)
            .current_dir(self.dir.path())
            .env("GH_TOKEN", &self.token)
            .env("GH_PROMPT_DISABLED", "1")
            .env("NO_COLOR", "1")
            .output()
            .expect("gh on PATH");
        if output.status.success() {
            return Ok(String::from_utf8(output.stdout).expect("UTF-8"));
        }
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        Err(format!("gh {args:?}: {stderr}"))
    }

    /// `gh <args>`: its output; a failure fails the test.
    fn gh(&self, args: &[&str]) -> String {
        self.try_gh(args)
            .unwrap_or_else(|failed| panic!("{failed}"))
    }

    /// `gh api <path>`, as JSON.
    fn api(&self, path: &str) -> Value {
        let text = self.gh(&["api", path]);
        serde_json::from_str(&text).expect("gh api prints JSON")
    }

    /// The commit GitHub's `branch` names.
    fn head(&self, branch: &str) -> String {
        let path = format!("repos/{}/git/ref/heads/{branch}", self.repo);
        let found = self.api(&path);
        found["object"]["sha"].as_str().expect("a sha").to_owned()
    }

    /// The changeset `store` imported GitHub's `branch` as, by a fetch's report.
    fn head_changeset(&self, store: &Store, branch: &str) -> Option<String> {
        let fetched = store.read(&["fetch", ORIGIN]).ok();
        assert_eq!(fetched["head"]["sha"], self.head(branch).as_str());
        fetched["head"]["changeset"].as_str().map(str::to_owned)
    }

    /// The message of commit `sha`.
    fn message(&self, sha: &str) -> String {
        let path = format!("repos/{}/commits/{sha}", self.repo);
        let commit = self.api(&path);
        commit["commit"]["message"]
            .as_str()
            .expect("a message")
            .to_owned()
    }

    /// Waits until a workflow run for `sha` finishes, and asserts that it succeeded.
    fn wait_for_workflow(&self, sha: &str) {
        let path = format!("repos/{}/actions/runs?head_sha={sha}", self.repo);
        let deadline = Instant::now() + WORKFLOW_WAIT;
        loop {
            let runs = self.api(&path);
            let listed = runs["workflow_runs"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let done: Vec<&Value> = listed
                .iter()
                .filter(|run| run["status"] == "completed")
                .collect();
            if !listed.is_empty() && done.len() == listed.len() {
                let failed: Vec<&Value> = done
                    .iter()
                    .copied()
                    .filter(|run| run["conclusion"] != "success")
                    .collect();
                assert!(
                    failed.is_empty(),
                    "the workflow on {sha} failed: {failed:?}"
                );
                return;
            }
            assert!(
                Instant::now() < deadline,
                "no finished workflow run for {sha} within {WORKFLOW_WAIT:?}"
            );
            std::thread::sleep(WORKFLOW_POLL);
        }
    }

    /// A pull request from `head` into `base`, merged by `method` (`merge`, `squash` or
    /// `rebase`); the commit GitHub's `base` names after it.
    fn pull_and_merge(&self, head: &str, base: &str, method: &str) -> String {
        // 1. The pull request.
        let title = format!("SwarmOps live run: {method}");
        let line = [
            "pr",
            "create",
            "--repo",
            &self.repo,
            "--base",
            base,
            "--head",
            head,
            "--title",
            &title,
            "--body",
            "Opened and merged by SwarmOps' live test.",
        ];
        let url = self.gh(&line);
        let url = url.trim();

        // 2. Its merge, tried again while GitHub computes whether it can.
        let flag = format!("--{method}");
        let line = ["pr", "merge", url, "--repo", &self.repo, &flag];
        let mut tries = 0;
        while let Err(failed) = self.try_gh(&line) {
            tries += 1;
            assert!(tries < MERGE_TRIES, "{failed}");
            std::thread::sleep(MERGE_PAUSE);
        }
        self.head(base)
    }

    /// Deletes the run's `branch`: only one under [`BRANCH_PREFIX`], never the default branch.
    fn delete(&self, branch: &str) -> Result<String, String> {
        assert!(branch.starts_with(BRANCH_PREFIX), "{branch}");
        let path = format!("repos/{}/git/refs/heads/{branch}", self.repo);
        self.try_gh(&["api", "-X", "DELETE", &path])
    }
}

/// The run's branches, deleted when the run ends, passed or failed.
struct Cleanup<'a> {
    /// The repository.
    github: &'a GitHub,
    /// The branches the run pushed.
    branches: Vec<String>,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        for branch in &self.branches {
            if let Err(failed) = self.github.delete(branch) {
                eprintln!("left {branch}: {failed}");
            }
        }
    }
}
