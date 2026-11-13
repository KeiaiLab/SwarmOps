//! Served git traffic under a swarm's load (the M17 design §7): git clients that each push a
//! branch of their own and fetch the trunk on an open-loop schedule, against `swarmops serve` or
//! git's own server, every command's outcome read off git's output.
//!
//! ```text
//!   GitClient n ── every 2 s (landing::slot) ──▶ commit clients/<n>.txt on release/client-<n>
//!       │                                        push HEAD:refs/heads/release/client-<n>
//!       │                                        fetch origin main
//!       └── each command run again while it ends retryable, at most LAND_RERUNS times
//!           one LandRecord per command: its attempts' Outcomes, its start (at_us), its latency
//! ```
//!
//! - [`outcome_of`]: how one run of git ended ([`Outcome`]): a refusal is retryable only when
//!   the error envelope `serve` writes (bare, or behind `remote: `) names a retryable code and
//!   gives a hint; any other failure, a refusal in a ref's report line among them, is
//!   [`Outcome::Other`].
//! - [`GitClient`]: one client in a clone of its own, through a [`Server`]; [`GitClient::run`]
//!   is its open loop.
//! - [`first_attempt`]: the share of commands that succeeded at their first attempt (G21).
//! - [`outcome_counts`]: the attempts counted record by record, each record held to the shape a
//!   command's reruns give it: the count a run's summary is checked against.
//!
//! The records are [`LandRecord`]s, so that [`Stats`](crate::landing::Stats) and its windows
//! count them as they count lands: `id` is the commit a push acknowledged (`ok` for a fetch),
//! `None` for a command that ended without succeeding.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use crate::landing::{LAND_RERUNS, LandRecord, Outcome, open_loop_lands, slot};
use crate::remote::GitServer;
use crate::serve::ServeSite;

/// The trunk the clients fetch.
pub const TRUNK: &str = "main";

/// What a fetch's record names as its `id` when it succeeded.
const FETCHED: &str = "ok";

/// The prefix git puts before a line the server wrote on its side band.
const REMOTE: &str = "remote: ";

/// Which server a client reaches, for the git commands it runs in that server's pinned
/// environment.
#[derive(Clone, Copy, Debug)]
pub enum Server<'s> {
    /// `swarmops serve`'s site.
    Serve(&'s ServeSite),
    /// git's own server.
    Git(&'s GitServer),
}

impl Server<'_> {
    /// A git command in the server's pinned client environment.
    fn command(&self) -> Command {
        match self {
            Server::Serve(site) => site.command(),
            Server::Git(server) => server.command(),
        }
    }
}

/// The command of a client's step a record is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// `git push origin HEAD:refs/heads/<its branch>`.
    Push,
    /// `git fetch origin main`.
    Fetch,
}

impl Step {
    /// Both, in the order a step runs them.
    pub const ALL: [Step; 2] = [Step::Push, Step::Fetch];

    /// Its name on the lines.
    pub fn name(self) -> &'static str {
        match self {
            Step::Push => "push",
            Step::Fetch => "fetch",
        }
    }
}

/// How the git run `output` ended: [`Outcome::Success`] for exit 0; [`Outcome::Retryable`] for
/// a failure whose stderr holds an error envelope (a line of its own, or behind `remote: `) with
/// `retryable: true`, an `E_*` code and a hint; [`Outcome::Other`] for any other failure.
pub fn outcome_of(output: &Output) -> Outcome {
    if output.status.success() {
        return Outcome::Success;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    for line in stderr.lines() {
        let line = line.strip_prefix(REMOTE).unwrap_or(line).trim();

        // 1. Only an envelope's line counts: `fatal: remote error: <CODE>: …` and a ref's
        //    report line carry no hint.
        let Ok(envelope) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let error = &envelope["error"];

        // 2. A retryable code, and a hint that says what to do.
        let code = error["code"].as_str().filter(|code| code.starts_with("E_"));
        let hint = error["hint"].as_str().filter(|hint| !hint.is_empty());
        let (Some(code), Some(hint), true) = (code, hint, error["retryable"] == true) else {
            continue;
        };
        return Outcome::Retryable {
            code: code.to_owned(),
            hint: Some(hint.to_owned()),
        };
    }
    Outcome::Other
}

/// The share of `records`' commands whose first attempt succeeded, from 0 to 1; 1 for none.
pub fn first_attempt(records: &[Vec<LandRecord>]) -> f64 {
    let all: Vec<&LandRecord> = records.iter().flatten().collect();
    if all.is_empty() {
        return 1.0;
    }
    let first = all
        .iter()
        .filter(|record| record.outcomes.first() == Some(&Outcome::Success))
        .count();
    count(first) / count(all.len())
}

/// What a run's commands came to, counted record by record ([`outcome_counts`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// The commands: one per record.
    pub commands: u64,
    /// The commands that succeeded.
    pub succeeded: u64,
    /// The attempts that ended retryable.
    pub retryable: u64,
    /// The attempts that ended in another failure.
    pub other: u64,
}

/// `records`' commands and attempts ([`Counts`]), each record held to the shape its command's
/// reruns give it; the error names the first record that has another, by client and command.
///
/// ```text
/// [Success]                       id set     one command, succeeded
/// [Retryable, Retryable, Success] id set     the same, two attempts retryable
/// [Retryable × (LAND_RERUNS + 1)] no id      given up: one command, none succeeded
/// [Other]                         no id      one other failure
/// ```
///
/// - at least one attempt, at most [`LAND_RERUNS`] reruns after the first;
/// - only a retryable attempt is followed by another;
/// - the record names what it succeeded with (`id`) exactly when its last attempt succeeded.
pub fn outcome_counts(records: &[Vec<LandRecord>]) -> Result<Counts, String> {
    let most = usize::try_from(LAND_RERUNS).map_or(usize::MAX, |reruns| reruns.saturating_add(1));
    let mut counts = Counts::default();
    for (client, commands) in records.iter().enumerate() {
        for (n, record) in commands.iter().enumerate() {
            let broken = |rule: &str| {
                let outcomes = &record.outcomes;
                format!("{rule}: client {client}'s command {n} has {outcomes:?}")
            };

            // 1. The attempts' shape.
            let Some((last, earlier)) = record.outcomes.split_last() else {
                return Err(broken("no attempt"));
            };
            if record.outcomes.len() > most {
                return Err(broken("too many attempts"));
            }
            let reran = |outcome: &Outcome| matches!(outcome, Outcome::Retryable { .. });
            if !earlier.iter().all(reran) {
                return Err(broken("an attempt after the command's end"));
            }
            let succeeded = *last == Outcome::Success;
            if succeeded != record.id.is_some() {
                let id = &record.id;
                return Err(broken(&format!("its id {id:?} against its last attempt")));
            }

            // 2. Its counts.
            counts.commands += 1;
            counts.succeeded += u64::from(succeeded);
            let retryable = record
                .outcomes
                .iter()
                .filter(|outcome| reran(outcome))
                .count();
            counts.retryable += u64::try_from(retryable).unwrap_or(u64::MAX);
            counts.other += u64::from(*last == Outcome::Other);
        }
    }
    Ok(counts)
}

/// A count as a float, exact far past any count of commands here.
fn count(n: usize) -> f64 {
    f64::from(u32::try_from(n).unwrap_or(u32::MAX))
}

/// The branch client `n` pushes: `release/client-<n>`, a name a store of FORMAT 1 takes.
pub fn client_branch(n: usize) -> String {
    format!("release/client-{n}")
}

/// What one client's open loop recorded: one list per [`Step`].
#[derive(Debug, Default)]
pub struct Traffic {
    /// Its pushes, in order.
    pub pushes: Vec<LandRecord>,
    /// Its fetches, in order.
    pub fetches: Vec<LandRecord>,
}

/// One git client in a clone of its own (`dir`, whose `origin` is the served repository): on
/// its own branch ([`client_branch`]), made from `origin/main` by [`GitClient::new`], each step
/// commits `clients/<n>.txt`, pushes the branch and fetches the trunk.
#[derive(Debug)]
pub struct GitClient<'s> {
    /// The server it reaches.
    server: Server<'s>,
    /// Its number in the run, from 0.
    n: usize,
    /// Its clone.
    dir: PathBuf,
    /// `GIT_SSH_COMMAND` for its connections.
    ssh: String,
    /// The commit its last acknowledged push set its branch to.
    acknowledged: Option<String>,
}

impl<'s> GitClient<'s> {
    /// Client `n` in the clone `dir` of `server`, connecting with `ssh`: its branch checked out
    /// at `origin/main`, nothing pushed yet.
    pub fn new(server: Server<'s>, n: usize, dir: &Path, ssh: &str) -> GitClient<'s> {
        let client = GitClient {
            server,
            n,
            dir: dir.to_owned(),
            ssh: ssh.to_owned(),
            acknowledged: None,
        };
        let start = format!("origin/{TRUNK}");
        let checkout = ["checkout", "--quiet", "-b", &client_branch(n), &start];
        let output = client.git(&checkout);
        assert!(output.status.success(), "client {n}: {output:?}");
        client
    }

    /// Its branch.
    pub fn branch(&self) -> String {
        client_branch(self.n)
    }

    /// The commit its last acknowledged push set its branch to; `None` before one.
    pub fn acknowledged(&self) -> Option<&str> {
        self.acknowledged.as_deref()
    }

    /// Its clone.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Its open loop: from `go`, one step at each of its slots ([`slot`], at `interval` for
    /// `duration`, drawn from `seed` as lander `who`'s), each record's start set.
    pub fn run(
        &mut self,
        go: Instant,
        (seed, who): (u64, usize),
        interval: Duration,
        duration: Duration,
    ) -> Traffic {
        let steps = open_loop_lands(seed, who, interval, duration);
        let mut traffic = Traffic::default();
        for step in 0..steps {
            let due = go + slot(seed, who, step, interval);
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
            let at = micros(go.elapsed());
            let (mut push, mut fetch) = self.step(step);
            push.at_us = at;
            fetch.at_us = at;
            traffic.pushes.push(push);
            traffic.fetches.push(fetch);
        }
        traffic
    }

    /// One step: a commit on its branch, its push, then a fetch of the trunk; their records.
    pub fn step(&mut self, step: usize) -> (LandRecord, LandRecord) {
        // 1. A commit of its own file.
        let file = format!("clients/{}.txt", self.n);
        let path = self.dir.join(&file);
        fs::create_dir_all(path.parent().expect("a parent")).expect("create clients/");
        fs::write(&path, format!("client {} step {step}\n", self.n)).expect("write a file");
        for args in [
            &["add", "--", file.as_str()][..],
            &["commit", "--quiet", "-m", file.as_str()],
        ] {
            let output = self.git(args);
            assert!(output.status.success(), "client {}: {output:?}", self.n);
        }
        let head = self.git(&["rev-parse", "--verify", "HEAD"]);
        let head = String::from_utf8_lossy(&head.stdout).trim().to_owned();

        // 2. Its push, then the fetch; a push that succeeded is acknowledged.
        let refspec = format!("HEAD:refs/heads/{}", self.branch());
        let push = self.command(&["push", "--quiet", "origin", refspec.as_str()], &head);
        if push.id.is_some() {
            self.acknowledged = Some(head);
        }
        let fetch = self.command(&["fetch", "--quiet", "origin", TRUNK], FETCHED);
        (push, fetch)
    }

    /// The heads `refs/heads/*` of `origin` as `git ls-remote` lists them, by branch name.
    pub fn remote_heads(&self) -> BTreeMap<String, String> {
        let output = self.git(&["ls-remote", "--heads", "origin"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "client {}: {stderr}", self.n);
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .filter_map(|(oid, name)| Some((name.strip_prefix("refs/heads/")?, oid)))
            .map(|(name, oid)| (name.to_owned(), oid.to_owned()))
            .collect()
    }

    /// `args` run until they succeed, end in another failure, or have been run again
    /// [`LAND_RERUNS`] times after a retryable refusal: the record, `id` set to `succeeded`
    /// when they succeeded.
    fn command(&self, args: &[&str], succeeded: &str) -> LandRecord {
        let started = Instant::now();
        let mut record = LandRecord::default();
        for _ in 0..=LAND_RERUNS {
            let outcome = outcome_of(&self.git(args));
            record.attempts += 1;
            record.outcomes.push(outcome.clone());
            // A retryable refusal runs again; a success or another failure ends the command.
            match outcome {
                Outcome::Success => {
                    record.id = Some(succeeded.to_owned());
                    break;
                }
                Outcome::Other => break,
                Outcome::Retryable { .. } => {}
            }
        }
        record.latency_us = micros(started.elapsed());
        record
    }

    /// git `args` in its clone, connecting with its stand-in: the output, whatever the exit.
    fn git(&self, args: &[&str]) -> Output {
        self.server
            .command()
            .env("GIT_SSH_COMMAND", &self.ssh)
            .current_dir(&self.dir)
            .args(args)
            .output()
            .expect("run git")
    }
}

/// `elapsed` in whole microseconds.
fn micros(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::{ExitStatus, Output};

    use super::{Counts, first_attempt, outcome_counts, outcome_of};
    use crate::landing::{LAND_RERUNS, LandRecord, Outcome};

    /// A git run that exited with `code` and wrote `stderr`.
    fn ran(code: i32, stderr: &str) -> Output {
        Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    /// A retryable refusal.
    fn refused() -> Outcome {
        Outcome::Retryable {
            code: "E_BUSY".to_owned(),
            hint: Some("connect again".to_owned()),
        }
    }

    /// A command's record: `outcomes` in order, its `id` set when the last one succeeded.
    fn command(outcomes: &[Outcome]) -> LandRecord {
        let succeeded = outcomes.last() == Some(&Outcome::Success);
        LandRecord {
            id: succeeded.then(|| "ok".to_owned()),
            outcomes: outcomes.to_vec(),
            ..LandRecord::default()
        }
    }

    /// Two clients' commands: two that succeeded at once, one after a refusal, one that ended
    /// in another failure after one, one given up after every rerun.
    fn two_clients() -> Vec<Vec<LandRecord>> {
        let given_up: Vec<Outcome> = (0..=LAND_RERUNS).map(|_| refused()).collect();
        vec![
            vec![
                command(&[Outcome::Success]),
                command(&[refused(), Outcome::Success]),
                command(&given_up),
            ],
            vec![
                command(&[Outcome::Success]),
                command(&[refused(), Outcome::Other]),
            ],
        ]
    }

    /// G21's share counts a command once, by its first attempt alone, over every client: two of
    /// five here, though three succeeded in the end; no command at all reads 1.
    #[test]
    fn the_first_attempt_share_counts_first_attempts_only() {
        let share = first_attempt(&two_clients());
        assert!((share - 0.4).abs() < 1e-12, "{share}");

        let none = first_attempt(&[Vec::new(), Vec::new()]);
        assert!((none - 1.0).abs() < 1e-12, "{none}");
        let refused_first = first_attempt(&[vec![command(&[refused(), Outcome::Success])]]);
        assert!(refused_first.abs() < 1e-12, "{refused_first}");
    }

    /// The counts are the commands' own: five commands, three succeeded, eleven attempts
    /// retryable (1 + 9 + 1), one other failure.
    #[test]
    fn outcomes_are_counted_record_by_record() {
        let counts = outcome_counts(&two_clients()).expect("well-formed records");
        let reruns = u64::from(LAND_RERUNS);
        assert_eq!(
            counts,
            Counts {
                commands: 5,
                succeeded: 3,
                retryable: reruns + 3,
                other: 1,
            }
        );
        assert_eq!(outcome_counts(&[]), Ok(Counts::default()));
    }

    /// A record that no command's reruns give is refused by its rule, naming the client and the
    /// command: no attempt, one rerun too many, an attempt after a success or another failure,
    /// an id without a last success, a last success without an id.
    #[test]
    fn a_record_no_command_gives_is_refused() {
        let too_many: Vec<Outcome> = (0..LAND_RERUNS + 2).map(|_| refused()).collect();
        let named = LandRecord {
            id: Some("ok".to_owned()),
            ..command(&[refused()])
        };
        let unnamed = LandRecord {
            id: None,
            ..command(&[Outcome::Success])
        };
        let cases = [
            (command(&[]), "no attempt: "),
            (command(&too_many), "too many attempts: "),
            (
                command(&[Outcome::Success, Outcome::Success]),
                "an attempt after the command's end: ",
            ),
            (
                command(&[Outcome::Other, Outcome::Success]),
                "an attempt after the command's end: ",
            ),
            (named, "its id Some(\"ok\") against its last attempt: "),
            (unnamed, "its id None against its last attempt: "),
        ];
        for (record, rule) in cases {
            let records = vec![vec![command(&[Outcome::Success])], vec![record]];
            let refusal = outcome_counts(&records).expect_err(rule);
            assert!(refusal.starts_with(rule), "{refusal}");
            assert!(refusal.contains("client 1's command 0 has ["), "{refusal}");
        }
    }

    /// A refusal counts as retryable only when its envelope names a retryable code and gives a
    /// hint: the same code without a hint, a hint without a retryable code, and a refusal in a
    /// ref's report line (which carries no hint) are other failures.
    #[test]
    fn a_refusal_without_a_hint_counts_as_other() {
        let busy = r#"{"ok":false,"error":{"code":"E_BUSY","message":"serve.lock is held","retryable":true,"hint":"connect again"}}"#;
        let bare = r#"{"ok":false,"error":{"code":"E_BUSY","message":"serve.lock is held","retryable":true}}"#;
        let auth = r#"{"ok":false,"error":{"code":"E_AUTH","message":"no grant","retryable":false,"hint":"ask the operator"}}"#;

        // 1. Success, and a retryable code with a hint, over ssh and behind `remote: `.
        assert_eq!(outcome_of(&ran(0, "")), Outcome::Success);
        let retryable = Outcome::Retryable {
            code: "E_BUSY".to_owned(),
            hint: Some("connect again".to_owned()),
        };
        let erred = format!("fatal: remote error: E_BUSY: serve.lock is held\n{busy}\n");
        assert_eq!(outcome_of(&ran(128, &erred)), retryable);
        let remote = format!("remote: {busy}\nerror: failed to push some refs\n");
        assert_eq!(outcome_of(&ran(1, &remote)), retryable);

        // 2. No hint, no retryable code, or no envelope: other.
        let no_hint = format!("fatal: remote error: E_BUSY: serve.lock is held\n{bare}\n");
        assert_eq!(outcome_of(&ran(128, &no_hint)), Outcome::Other);
        assert_eq!(outcome_of(&ran(128, auth)), Outcome::Other);
        let report = " ! [remote rejected] HEAD -> release/x (E_BUSY: the store is busy)\n";
        assert_eq!(outcome_of(&ran(1, report)), Outcome::Other);
        assert_eq!(
            outcome_of(&ran(128, "fatal: the remote end hung up\n")),
            Outcome::Other
        );
    }
}
