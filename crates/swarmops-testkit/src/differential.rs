//! A differential suite against git's own server (the M17 design §2): one scripted session of
//! stock git runs against `swarmops serve` and against `git upload-pack`/`git receive-pack` on a
//! bare copy holding the same history, both through an ssh stand-in. Each step's exit status, the
//! refs it reports and the client's objects afterwards are compared; what differs on purpose is
//! listed with a reason in `docs/operate/serve-differences.md` ([`listed`] reads its table).
//!
//! ```text
//!   Session (the fixture's repository, the script)
//!     ├── run_against_serve(ServeSite)  ssh://…/differential.git ── swarmops serve
//!     └── run_against_git(GitServer)    ssh://…/differential.git ── git upload-pack / receive-pack
//!   Transcript, Transcript ── compare(listed) ── one message per unlisted difference
//! ```
//!
//! The served store [`STORE`] must hold the fixture's [`BRANCHES`] and [`TAGS`] (as `tags/<tag>`)
//! before [`Session::run_against_serve`]: the CLI's tests import them, as only they can run the
//! binary. [`Session::run_against_git`] makes the bare copy itself, with those refs alone (the
//! store keeps no annotated tag, so a mirror would differ by it).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

use crate::child;
use crate::remote::{GitServer, SshMode, path_str};
use crate::serve::ServeSite;

/// The store, and the bare repository, the session runs against.
pub const STORE: &str = "differential";

/// Who the session connects to `serve` as.
pub const IDENTITY: &str = "person/ada";

/// The branches of the `merges` fixture both servers hold.
pub const BRANCHES: [&str; 8] = [
    "main", "feature", "one", "two", "three", "side", "cross-a", "cross-b",
];

/// The `merges` fixture's lightweight tags both servers hold.
pub const TAGS: [&str; 1] = ["light"];

/// The served store's access file: [`IDENTITY`] pushes and deletes [`NEW_BRANCH`]'s family and
/// lands on `main`; `main` itself moves by a land alone.
const ACCESS: &str = r#"version = 1

[[grant]]
who = ["person/ada"]
push = ["topic/**"]
force = ["topic/**"]
land = ["main"]
"#;

/// The branch [`Step::PushNewBranch`] makes and [`Step::DeleteBranch`] deletes.
const NEW_BRANCH: &str = "topic/new";

/// The date of every commit the client writes, so that both sessions write the same commits.
const DATE: &str = "1700000000 +0000";

/// The clone every step but the shallow ones works in.
const FULL: &str = "full";

/// The clone of the shallow steps.
const SHALLOW: &str = "shallow";

/// A command of the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// `git clone` into the full clone.
    Clone,
    /// `git clone --depth <depth>` into the shallow clone.
    ShallowClone {
        /// The commits it asks for.
        depth: u32,
    },
    /// `git fetch --deepen=<by>` in the shallow clone.
    Deepen {
        /// The commits it adds below the shallow ones.
        by: u32,
    },
    /// `git fetch` in the full clone.
    Fetch,
    /// `git ls-remote`, the refs under `prefix`.
    LsRemote {
        /// The refs' prefix, such as `refs/heads/`.
        prefix: String,
    },
    /// A new commit on `origin/main` pushed as a new branch.
    PushNewBranch,
    /// A commit beside `main`'s tip forced onto `main`, which both servers refuse.
    PushNonFastForward,
    /// The new branch deleted by a push.
    DeleteBranch,
    /// A new commit on `origin/main` pushed to `refs/land/main`.
    PushLand,
}

impl Step {
    /// Its variant's name, as the table of differences names it.
    pub fn name(&self) -> &'static str {
        match self {
            Step::Clone => "Clone",
            Step::ShallowClone { .. } => "ShallowClone",
            Step::Deepen { .. } => "Deepen",
            Step::Fetch => "Fetch",
            Step::LsRemote { .. } => "LsRemote",
            Step::PushNewBranch => "PushNewBranch",
            Step::PushNonFastForward => "PushNonFastForward",
            Step::DeleteBranch => "DeleteBranch",
            Step::PushLand => "PushLand",
        }
    }
}

/// The session's steps, in order: the reads, then the pushes, the land last so that its
/// difference stays its own.
pub fn script() -> Vec<Step> {
    vec![
        Step::Clone,
        Step::LsRemote {
            prefix: "refs/heads/".to_owned(),
        },
        Step::LsRemote {
            prefix: "refs/tags/".to_owned(),
        },
        Step::ShallowClone { depth: 1 },
        Step::Deepen { by: 2 },
        Step::PushNewBranch,
        Step::Fetch,
        Step::PushNonFastForward,
        Step::DeleteBranch,
        Step::Fetch,
        Step::PushLand,
    ]
}

/// What one session's steps did, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transcript {
    /// One outcome per step.
    pub steps: Vec<Outcome>,
}

/// What one step did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// The step.
    pub step: Step,
    /// git's exit status (-1 for a signal).
    pub exit: i32,
    /// The refs it reports, by name: a clone's or fetch's client refs (`for-each-ref`), the
    /// listed refs of `ls-remote`, and a push's status lines (`--porcelain`, the destination to
    /// `<flag> <summary>`) followed by the refs the server advertises afterwards
    /// (`advertised <name>`).
    pub refs: Vec<(String, String)>,
    /// The client's sorted `rev-list --objects --all`.
    pub objects: Vec<String>,
}

/// A difference `serve` has on purpose: one row of the table of
/// `docs/operate/serve-differences.md`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Difference {
    /// The [`Step::name`] it shows in.
    pub command: String,
    /// `exit`, `refs` or `objects`.
    pub field: String,
    /// What differs; its first code span is the ref or object it covers (`*` any characters).
    pub difference: String,
    /// Why.
    pub reason: String,
}

/// A scripted git session over a committed fixture's history.
#[derive(Debug)]
pub struct Session {
    /// The fixture's repository, the bare copy's source.
    fixture: PathBuf,
    /// The steps.
    script: Vec<Step>,
}

/// The server a session runs against.
#[derive(Clone, Copy)]
enum Target<'a> {
    /// `swarmops serve`, through [`ServeSite::ssh_command`].
    Serve(&'a ServeSite),
    /// git's own server, through [`GitServer::ssh_command`].
    Git(&'a GitServer),
}

/// The client side of one session: its work directory and how it reaches the server.
struct Client<'a> {
    /// The server.
    target: Target<'a>,
    /// The clones.
    work: TempDir,
    /// `GIT_SSH_COMMAND`.
    ssh: String,
    /// The repository's URL.
    url: String,
}

impl Session {
    /// The [`script`] over `fixture`, a committed fixture's repository (testkit `git`).
    pub fn new(fixture: &Path) -> Session {
        Session {
            fixture: fixture.to_owned(),
            script: script(),
        }
    }

    /// The session against `site`, whose store [`STORE`] holds the fixture's refs; its access
    /// file is written here.
    pub fn run_against_serve(&self, site: &ServeSite) -> Transcript {
        fs::write(site.store(STORE).join("access.toml"), ACCESS).expect("write access.toml");
        self.run(Target::Serve(site))
    }

    /// The session against `server`, after a bare copy of the fixture's refs is made there. Like
    /// the store's `main`, its `main` refuses a push that is not a fast-forward.
    pub fn run_against_git(&self, server: &GitServer) -> Transcript {
        // 1. The bare copy: the branches and tags the store holds, nothing else.
        let bare = server.create(STORE);
        let refspecs = BRANCHES
            .iter()
            .map(|branch| format!("refs/heads/{branch}"))
            .chain(TAGS.iter().map(|tag| format!("refs/tags/{tag}")))
            .map(|name| format!("{name}:{name}"));
        let mut push = server.command();
        push.arg("--git-dir")
            .arg(&self.fixture)
            .args(["push", "--quiet"])
            .arg(&bare)
            .args(refspecs);
        succeed(&mut push);

        // 2. main refuses a non-fast-forward, as a hosting service's protected branch does.
        let mut deny = server.command();
        deny.arg("--git-dir")
            .arg(&bare)
            .args(["config", "receive.denyNonFastForwards", "true"]);
        succeed(&mut deny);
        self.run(Target::Git(server))
    }

    /// Every step against `target`.
    fn run(&self, target: Target<'_>) -> Transcript {
        let client = Client::new(target);
        let steps = self.script.iter().map(|step| client.step(step)).collect();
        Transcript { steps }
    }
}

impl<'a> Client<'a> {
    /// A fresh work directory against `target`.
    fn new(target: Target<'a>) -> Client<'a> {
        let (ssh, url) = match target {
            Target::Serve(site) => (site.ssh_command(IDENTITY), site.ssh_url(STORE)),
            Target::Git(server) => (server.ssh_command(SshMode::Serve), server.ssh_url(STORE)),
        };
        Client {
            target,
            work: tempfile::tempdir().expect("create the session's directory"),
            ssh,
            url,
        }
    }

    /// `step` run, and what it did.
    fn step(&self, step: &Step) -> Outcome {
        let full = self.work.path().join(FULL);
        let shallow = self.work.path().join(SHALLOW);
        match step {
            Step::Clone => self.read(step, &full, &["clone", "--quiet", &self.url, FULL]),
            Step::ShallowClone { depth } => {
                let depth = format!("--depth={depth}");
                self.read(
                    step,
                    &shallow,
                    &["clone", "--quiet", &depth, &self.url, SHALLOW],
                )
            }
            Step::Deepen { by } => {
                let by = format!("--deepen={by}");
                self.read(step, &shallow, &["-C", SHALLOW, "fetch", "--quiet", &by])
            }
            Step::Fetch => self.read(step, &full, &["-C", FULL, "fetch", "--quiet"]),
            Step::LsRemote { prefix } => self.ls_remote(step, prefix),
            Step::PushNewBranch => {
                self.commit(NEW_BRANCH, "origin/main", "new.txt");
                self.push(step, &[NEW_BRANCH])
            }
            Step::PushNonFastForward => {
                self.commit("rewrite", "origin/main~1", "rewrite.txt");
                self.push(step, &["--force", "rewrite:refs/heads/main"])
            }
            Step::DeleteBranch => self.push(step, &[&format!(":refs/heads/{NEW_BRANCH}")]),
            Step::PushLand => {
                self.commit("land", "origin/main", "land.txt");
                self.push(step, &["HEAD:refs/land/main"])
            }
        }
    }

    /// A clone or fetch: `args` in the work directory; the refs and objects of `clone` after.
    fn read(&self, step: &Step, clone: &Path, args: &[&str]) -> Outcome {
        let output = self.git(args);
        Outcome {
            step: step.clone(),
            exit: exit(&output),
            refs: self.client_refs(clone),
            objects: self.objects(clone),
        }
    }

    /// `ls-remote`: the refs under `prefix`; the full clone's objects.
    fn ls_remote(&self, step: &Step, prefix: &str) -> Outcome {
        let output = self.git(&["ls-remote", &self.url]);
        let refs = listed_refs(&text(&output))
            .into_iter()
            .filter(|(name, _)| name.starts_with(prefix))
            .collect();
        Outcome {
            step: step.clone(),
            exit: exit(&output),
            refs,
            objects: self.objects(&self.work.path().join(FULL)),
        }
    }

    /// `git push --porcelain origin <args>` in the full clone: its status lines, then the refs
    /// the server advertises.
    fn push(&self, step: &Step, args: &[&str]) -> Outcome {
        // 1. The push, its status line per ref.
        let line = [&["-C", FULL, "push", "--porcelain", "origin"][..], args].concat();
        let output = self.git(&line);
        let mut refs = porcelain(&text(&output));

        // 2. What the server holds afterwards.
        let advertised = self.git(&["ls-remote", &self.url]);
        let advertised = listed_refs(&text(&advertised))
            .into_iter()
            .map(|(name, oid)| (format!("advertised {name}"), oid));
        refs.extend(advertised);
        Outcome {
            step: step.clone(),
            exit: exit(&output),
            refs,
            objects: self.objects(&self.work.path().join(FULL)),
        }
    }

    /// A commit of `file` on `base` in the full clone, checked out as `branch`.
    fn commit(&self, branch: &str, base: &str, file: &str) {
        let full = self.work.path().join(FULL);
        self.ok(&["-C", FULL, "checkout", "--quiet", "-B", branch, base]);
        fs::write(full.join(file), format!("{file}\n")).expect("write a client file");
        self.ok(&["-C", FULL, "add", file]);
        self.ok(&["-C", FULL, "commit", "--quiet", "-m", file]);
    }

    /// The client's refs in `clone`, by name; none when it does not exist.
    fn client_refs(&self, clone: &Path) -> Vec<(String, String)> {
        if !clone.exists() {
            return Vec::new();
        }
        let format = "--format=%(refname)\t%(objectname)";
        let output = self.ok(&["-C", path_str(clone), "for-each-ref", format]);
        text(&output)
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .map(|(name, oid)| (name.to_owned(), oid.to_owned()))
            .collect()
    }

    /// The sorted `rev-list --objects --all` of `clone`; none when it does not exist.
    fn objects(&self, clone: &Path) -> Vec<String> {
        if !clone.exists() {
            return Vec::new();
        }
        let output = self.ok(&["-C", path_str(clone), "rev-list", "--objects", "--all"]);
        let mut objects: Vec<String> = text(&output).lines().map(str::to_owned).collect();
        objects.sort();
        objects
    }

    /// git with `args` in the work directory, which must succeed.
    fn ok(&self, args: &[&str]) -> Output {
        let output = self.git(args);
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// git with `args` in the work directory: the client's pinned environment, the ssh
    /// stand-in, and fixed commit dates.
    fn git(&self, args: &[&str]) -> Output {
        let mut command = match self.target {
            Target::Serve(site) => site.command(),
            Target::Git(server) => server.command(),
        };
        command
            .current_dir(self.work.path())
            .env("GIT_SSH_COMMAND", &self.ssh)
            .env("GIT_AUTHOR_DATE", DATE)
            .env("GIT_COMMITTER_DATE", DATE)
            .args(args);
        child::run(&mut command)
    }
}

/// Every difference between `serve` and `git` that no row of `listed` covers, one message each,
/// naming the step.
pub fn compare(serve: &Transcript, git: &Transcript, listed: &[Difference]) -> Vec<String> {
    let mut failures = Vec::new();
    if serve.steps.len() != git.steps.len() {
        let (s, g) = (serve.steps.len(), git.steps.len());
        failures.push(format!("serve ran {s} steps, git {g}"));
    }

    for (at, (s, g)) in serve.steps.iter().zip(&git.steps).enumerate() {
        let name = format!("step {at} ({:?})", s.step);

        // 1. The same command on both sides, or nothing else is comparable.
        if s.step != g.step {
            failures.push(format!("{name}: git ran {:?}", g.step));
            continue;
        }
        let covered = |field: &str, key: &str| {
            listed
                .iter()
                .any(|row| row.command == s.step.name() && row.field == field && covers(row, key))
        };

        // 2. The exit status.
        if s.exit != g.exit && !covered("exit", "exit") {
            failures.push(format!("{name}: exit: serve {}, git {}", s.exit, g.exit));
        }

        // 3. Each ref, by name.
        let (s_refs, g_refs) = (by_name(&s.refs), by_name(&g.refs));
        let names: BTreeSet<&str> = s_refs.keys().chain(g_refs.keys()).copied().collect();
        for key in names {
            let (sv, gv) = (s_refs.get(key), g_refs.get(key));
            if sv == gv || covered("refs", key) {
                continue;
            }
            let (sv, gv) = (shown(sv), shown(gv));
            failures.push(format!("{name}: ref {key}: serve {sv}, git {gv}"));
        }

        // 4. Each object the client holds on one side alone.
        let s_objects: BTreeSet<&String> = s.objects.iter().collect();
        let g_objects: BTreeSet<&String> = g.objects.iter().collect();
        for object in s_objects.symmetric_difference(&g_objects) {
            if covered("objects", object) {
                continue;
            }
            let side = if s_objects.contains(object) {
                "serve"
            } else {
                "git"
            };
            failures.push(format!("{name}: object {object}: on {side} alone"));
        }
    }
    failures
}

/// The rows of the table in `doc` (the differences page): command, field, difference, reason.
pub fn listed(doc: &str) -> Vec<Difference> {
    let rows = doc
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('|'))
        .map(|line| {
            let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
            cells
        })
        .filter(|cells| cells.len() == 4 && cells[0] != "Command")
        .filter(|cells| !cells[0].chars().all(|c| c == '-' || c == ':'));
    rows.map(|cells| Difference {
        command: cells[0].trim_matches('`').to_owned(),
        field: cells[1].trim_matches('`').to_owned(),
        difference: cells[2].to_owned(),
        reason: cells[3].to_owned(),
    })
    .collect()
}

/// Whether `row` covers `key`: its difference's first code span matches it, `*` standing for
/// any characters. An exit row covers the exit status alone.
fn covers(row: &Difference, key: &str) -> bool {
    if row.field == "exit" {
        return true;
    }
    let Some(pattern) = row.difference.split('`').nth(1) else {
        return false;
    };
    glob(pattern, key)
}

/// Whether `text` matches `pattern`, whose `*` stands for any characters.
fn glob(pattern: &str, text: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or("");
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    let parts: Vec<&str> = parts.collect();
    let Some((last, middle)) = parts.split_last() else {
        return rest.is_empty();
    };

    // Each middle part at its first place after the one before; the last at the end.
    for part in middle {
        let Some(at) = rest.find(part) else {
            return false;
        };
        rest = &rest[at + part.len()..];
    }
    rest.ends_with(last)
}

/// `refs` by name.
fn by_name(refs: &[(String, String)]) -> BTreeMap<&str, &str> {
    refs.iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect()
}

/// A ref's value for a message: `absent` when there is none.
fn shown(value: Option<&&str>) -> String {
    value.map_or_else(|| "absent".to_owned(), |value| (*value).to_owned())
}

/// The `<oid>\t<name>` lines of `ls-remote`, by name.
fn listed_refs(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(oid, name)| (name.to_owned(), oid.to_owned()))
        .collect()
}

/// The status lines of `push --porcelain` (`<flag>\t<from>:<to>\t<summary>`): the destination to
/// `<flag> <summary>`.
fn porcelain(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let (flag, refs, summary) = (fields.next()?, fields.next()?, fields.next()?);
            let (_, to) = refs.split_once(':')?;
            Some((to.to_owned(), format!("{flag} {summary}")))
        })
        .collect()
}

/// git's exit status, -1 for a signal.
fn exit(output: &Output) -> i32 {
    output.status.code().unwrap_or(-1)
}

/// What git printed on stdout.
fn text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Runs `command`, which must succeed.
fn succeed(command: &mut Command) {
    let output = child::run(command);
    assert!(
        output.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(test)]
mod tests {
    use super::{Difference, glob, listed};

    /// `*` stands for any characters, anywhere in the pattern.
    #[test]
    fn glob_matches_stars() {
        assert!(glob("refs/land/*", "refs/land/main"));
        assert!(glob("advertised refs/*/main", "advertised refs/land/main"));
        assert!(glob("refs/heads/main", "refs/heads/main"));
        assert!(!glob("refs/heads/main", "refs/heads/mainline"));
        assert!(!glob("refs/land/*", "refs/heads/main"));
    }

    /// The table's rows, the header and the separator passed over.
    #[test]
    fn listed_reads_the_table() {
        let doc = "# T\n\n| Command | Field | Difference | Reason |\n| --- | --- | --- | --- |\n\
                   | `PushLand` | `refs` | `refs/land/*`: absent | lands |\n";
        let row = Difference {
            command: "PushLand".to_owned(),
            field: "refs".to_owned(),
            difference: "`refs/land/*`: absent".to_owned(),
            reason: "lands".to_owned(),
        };
        assert_eq!(listed(doc), vec![row]);
    }
}
