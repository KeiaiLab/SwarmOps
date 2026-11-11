//! `swarmops serve`'s records (the M17 design §6): one JSON line per served request in the
//! store's `git/serve-records.jsonl`, saying where the connection's time went. A slow reader's
//! connection is spent sending, a cold clone's preparing; no record holds a host path or a
//! secret of the serving account's environment, and every key a record writes is a property of
//! `ServeRecord`'s published schema.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use swarmops_testkit::git::fixture;
use swarmops_testkit::serve::ServeSite;
use tempfile::TempDir;

use crate::support::{self, Swarmops};

/// The agent that clones, fetches and pushes.
const AGENT: &str = "person/ada";

/// An agent the access file names and grants nothing: its clone is refused.
const STRANGER: &str = "person/bob";

/// The agent that writes the served store directly, through the command line.
const WRITER: &str = "agent/worker-1";

/// The store every site serves.
const STORE: &str = "acme";

/// The served store's access file: [`AGENT`] pushes `main`, [`STRANGER`] is granted nothing.
const ACCESS: &str = r#"version = 1

[[grant]]
who = ["person/ada"]
push = ["main"]

[[grant]]
who = ["person/bob"]
"#;

/// The records' file, below the store's root.
const RECORDS: &str = "git/serve-records.jsonl";

/// The variable planted in the serving account's environment, which no record may hold.
const SECRET_PROBE: &str = "SWARMOPS_SECRET_PROBE";

/// Its value.
const SECRET: &str = "probe-5c1e7a0d9b3f";

/// The size of the incompressible file the big history holds: a pack of about this size.
const BIG_FILE_BYTES: usize = 6 << 20;

/// What the throttled reader reads at once, and how long it sleeps after: about 1 MiB/s.
const THROTTLE_CHUNK: usize = 16 << 10;
const THROTTLE_SLEEP: &str = "0.015";

/// The share of a slow reader's connection that is sending, at least, in percent.
const SEND_PERCENT: u64 = 90;

/// How long a record may take to appear after its client returned.
const RECORD_WAIT: Duration = Duration::from_secs(10);

/// The hooks' splits, below the store's root.
const SPLIT_DIR: &str = "git/serve-split";

/// The age of a split a dead `serve` left: past any bound `serve` sweeps at.
const DEAD_SPLIT_AGE: Duration = Duration::from_hours(7 * 24);

/// A served root with the store [`STORE`] under [`ACCESS`], and a directory for clients.
struct Site {
    /// The root, its stand-ins and the client git.
    served: ServeSite,
    /// The clients' working directories.
    work: TempDir,
}

impl Site {
    /// The store holding the `linear` fixture as `main`, served with `env` added to the serving
    /// account's environment.
    fn linear(env: &[(&str, &str)]) -> Site {
        let site = Site::empty(env);
        let linear = fixture("linear");
        site.import(linear.repo.as_deref().unwrap());
        site
    }

    /// The store holding one commit of a [`BIG_FILE_BYTES`] file of noise as `main`.
    fn big() -> Site {
        let site = Site::empty(&[]);
        let source = site.big_source();
        site.import(&source);
        site
    }

    /// An empty store under [`ACCESS`].
    fn empty(env: &[(&str, &str)]) -> Site {
        let served = ServeSite::new::<Swarmops>().with_env(env);
        served.init(STORE);
        let site = Site {
            served,
            work: TempDir::new().unwrap(),
        };
        site.grant();
        site
    }

    /// Writes [`ACCESS`] as the store's access file.
    fn grant(&self) {
        fs::write(self.served.store(STORE).join("access.toml"), ACCESS).unwrap();
    }

    /// The `git import` of `source`'s `HEAD` into `main`, as [`WRITER`], with the access file
    /// set aside, so that no preparation runs beside the test's connections.
    fn import(&self, source: &Path) {
        let access = self.served.store(STORE).join("access.toml");
        fs::remove_file(&access).unwrap();
        let store = self.served.store(STORE);
        let line = [
            "--repo",
            store.to_str().unwrap(),
            "--as",
            WRITER,
            "git",
            "import",
            source.to_str().unwrap(),
            "--ref",
            "HEAD",
            "--into",
            "main",
        ];
        support::run(&line, &[]).ok();
        self.grant();
    }

    /// A git repository of one commit holding [`BIG_FILE_BYTES`] of noise, which no
    /// compression shrinks.
    fn big_source(&self) -> PathBuf {
        let dir = self.work.path().join("big-source");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("noise.bin"), noise(BIG_FILE_BYTES)).unwrap();
        for args in [
            &["init", "--quiet", "--initial-branch", "main"][..],
            &["add", "noise.bin"],
            &["commit", "--quiet", "-m", "noise"],
        ] {
            let output = self
                .served
                .command()
                .current_dir(&dir)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                text(&output.stderr)
            );
        }
        dir.join(".git")
    }

    /// The store's root.
    fn store(&self) -> PathBuf {
        self.served.store(STORE)
    }

    /// git with `args` in `dir` through the ssh command `ssh`; what it did.
    fn git_via(&self, ssh: &str, dir: &Path, args: &[&str]) -> Output {
        self.served
            .command()
            .env("GIT_SSH_COMMAND", ssh)
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap()
    }

    /// git with `args` in `dir` as `who`, which must succeed.
    fn git_as(&self, who: &str, dir: &Path, args: &[&str]) {
        let output = self.git_via(&self.served.ssh_command(who), dir, args);
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            text(&output.stderr)
        );
    }

    /// The store cloned into `name` as [`AGENT`] through `ssh`; the clone's directory.
    fn clone_via(&self, ssh: &str, name: &str) -> PathBuf {
        let url = self.served.ssh_url(STORE);
        let output = self.git_via(ssh, self.work.path(), &["clone", "--quiet", &url, name]);
        assert!(
            output.status.success(),
            "git clone: {}",
            text(&output.stderr)
        );
        self.work.path().join(name)
    }

    /// The store cloned into `name` as [`AGENT`].
    fn clone(&self, name: &str) -> PathBuf {
        self.clone_via(&self.served.ssh_command(AGENT), name)
    }

    /// An ssh command whose client reads the server's output at about 1 MiB/s: the stand-in of
    /// [`AGENT`], its stdout through a pipe read [`THROTTLE_CHUNK`] at a time.
    fn throttled_ssh(&self) -> String {
        let dir = self.work.path().join("throttle");
        fs::create_dir(&dir).unwrap();
        let chunk = dir.join("chunk");
        let script = format!(
            "#!/bin/sh\n\
             {ssh} \"$@\" | while :; do\n\
             \x20 dd bs={THROTTLE_CHUNK} count=1 status=none of='{chunk}'\n\
             \x20 [ -s '{chunk}' ] || break\n\
             \x20 cat '{chunk}'\n\
             \x20 sleep {THROTTLE_SLEEP}\n\
             done\n",
            ssh = self.served.ssh_command(AGENT),
            chunk = chunk.to_str().unwrap(),
        );
        // Named ssh, so that git takes it for OpenSSH and does not probe it.
        let program = dir.join("ssh");
        fs::write(&program, script).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        format!("'{}'", program.to_str().unwrap())
    }

    /// The records written so far, waiting up to [`RECORD_WAIT`] until there are `count`.
    fn records(&self, count: usize) -> Vec<Value> {
        let started = Instant::now();
        loop {
            let lines = self.lines();
            if lines.len() >= count || started.elapsed() >= RECORD_WAIT {
                let records: Vec<Value> = lines
                    .iter()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                assert_eq!(records.len(), count, "{lines:#?}");
                return records;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The lines of the records' file, none when it is missing.
    fn lines(&self) -> Vec<String> {
        let text = fs::read_to_string(self.store().join(RECORDS)).unwrap_or_default();
        text.lines().map(str::to_owned).collect()
    }

    /// A commit of `path` in the clone `dir`, pushed to `main` as [`AGENT`].
    fn push(&self, dir: &Path, path: &str) {
        self.commit(dir, path, &format!("{path}\n"));
        self.git_as(AGENT, dir, &["push", "--quiet", "origin", "main"]);
    }

    /// A commit of `path` holding `content` in the clone `dir`, as [`AGENT`].
    fn commit(&self, dir: &Path, path: &str, content: &str) {
        fs::write(dir.join(path), content).unwrap();
        self.git_as(AGENT, dir, &["add", path]);
        self.git_as(AGENT, dir, &["commit", "--quiet", "-m", path]);
    }

    /// A split file `name` in the store, last written `age` ago.
    fn plant_split(&self, name: &str, age: Duration) -> PathBuf {
        let dir = self.store().join(SPLIT_DIR);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, "{}\n").unwrap();
        let file = fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::now() - age).unwrap();
        path
    }
}

/// `bytes` as text.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `len` bytes of xorshift noise.
fn noise(len: usize) -> Vec<u8> {
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// A record's milliseconds of `field`.
fn ms(record: &Value, field: &str) -> u64 {
    record[field]
        .as_u64()
        .unwrap_or_else(|| panic!("{field} in {record}"))
}

/// Which of `needles` `line` holds.
fn findings<'a>(line: &str, needles: &[&'a str]) -> Vec<&'a str> {
    needles
        .iter()
        .copied()
        .filter(|needle| line.contains(needle))
        .collect()
}

/// A client reading at about 1 MiB/s: the cached clone's connection is spent sending.
#[test]
fn a_slow_reader_shows_send_time() {
    // 1. The pack made and cached by a first clone, read at full speed.
    let site = Site::big();
    site.clone("warm");

    // 2. The same clone, read slowly.
    let ssh = site.throttled_ssh();
    site.clone_via(&ssh, "slow");

    // 3. Its record: a clone of about the file's size, most of it sending.
    let records = site.records(2);
    let slow = &records[1];
    assert_eq!(slow["service"], "clone", "{slow}");
    assert_eq!(slow["exit"], 0, "{slow}");
    assert!(ms(slow, "bytes") >= BIG_FILE_BYTES as u64, "{slow}");
    let (prepare, send) = (ms(slow, "prepare_ms"), ms(slow, "send_ms"));
    assert!(
        send * 100 > (prepare + send) * SEND_PERCENT,
        "send under {SEND_PERCENT} % of the connection: {slow}"
    );
}

/// The first clone exports the history and produces the pack before its first byte; a repeat
/// finds both done.
#[test]
fn a_cold_clone_prepares_longer_than_a_repeat() {
    let site = Site::big();
    site.clone("cold");
    site.clone("repeat");
    let records = site.records(2);
    let (cold, repeat) = (&records[0], &records[1]);
    for record in &records {
        assert_eq!(record["service"], "clone", "{record}");
        assert_eq!(record["identity"], AGENT, "{record}");
    }
    assert!(
        ms(cold, "prepare_ms") > ms(repeat, "prepare_ms"),
        "cold {cold}, repeat {repeat}"
    );
}

/// No record of a clone, a fetch, a push or a refusal holds the store's path, the root, the
/// serving account's `HOME`, or the value of a variable of its environment.
#[test]
fn records_hold_no_path_or_secret() {
    // 1. A clone, a fetch, a push and a refused clone, served with the probe planted.
    let site = Site::linear(&[(SECRET_PROBE, SECRET)]);
    let dir = site.clone("one");
    site.git_as(AGENT, &dir, &["fetch", "--quiet", "origin"]);
    site.push(&dir, "pushed.txt");
    let url = site.served.ssh_url(STORE);
    let refused = site.git_via(
        &site.served.ssh_command(STRANGER),
        site.work.path(),
        &["clone", "--quiet", &url, "refused"],
    );
    assert!(!refused.status.success(), "{}", text(&refused.stderr));

    // 2. Their records: one each, the refusal with its code.
    let records = site.records(4);
    let services: Vec<&str> = (records.iter())
        .map(|record| record["service"].as_str().unwrap())
        .collect();
    assert_eq!(
        services,
        ["clone", "fetch", "push", "fetch"],
        "{records:#?}"
    );
    assert!(records[3]["code"].is_string(), "{}", records[3]);
    assert_eq!(records[3]["identity"], STRANGER, "{}", records[3]);
    for record in &records[..3] {
        assert!(record.get("code").is_none(), "{record}");
    }

    // 3. None holds a path or the secret.
    let root = site.served.root();
    let home = root.parent().unwrap().join("home");
    let store = site.store();
    let needles = [
        store.to_str().unwrap(),
        root.to_str().unwrap(),
        home.to_str().unwrap(),
        SECRET,
    ];
    for line in site.lines() {
        assert_eq!(findings(&line, &needles), Vec::<&str>::new(), "{line}");
    }

    // 4. The self-check: a record with the root planted fails the same scan.
    let planted = serde_json::json!({"identity": root.to_str().unwrap()}).to_string();
    assert_eq!(findings(&planted, &needles), [root.to_str().unwrap()]);
}

/// Each key of a written record is a property of `ServeRecord`'s published schema, and a
/// record reads as one.
#[test]
fn every_record_field_is_in_the_schema() {
    // 1. The schema's properties.
    let schema = support::run(&["schema", "ServeRecord"], &[]).ok();
    let properties: BTreeSet<String> = schema["ServeRecord"]["properties"]
        .as_object()
        .unwrap_or_else(|| panic!("ServeRecord's properties: {schema}"))
        .keys()
        .cloned()
        .collect();

    // 2. A clone and a push, recorded.
    let before = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let site = Site::linear(&[]);
    let dir = site.clone("one");
    site.push(&dir, "pushed.txt");
    let records = site.records(2);
    let after = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

    // 3. Each key in the schema, each record a ServeRecord started within the test.
    for record in &records {
        let keys = record.as_object().unwrap().keys();
        for key in keys {
            assert!(properties.contains(key), "{key} not in {properties:?}");
        }
        serde_json::from_value::<swarmops_api::ServeRecord>(record.clone()).unwrap();
        let at = u128::from(ms(record, "at"));
        assert!(
            (before.as_millis()..=after.as_millis()).contains(&at),
            "{record}"
        );
    }
    assert_eq!(records[1]["service"], "push", "{}", records[1]);
}

/// A split a dead `serve` left behind is removed by the next connection's `serve`; a recent
/// one, which a live connection may still read, stays.
#[test]
fn a_stale_split_is_removed_at_the_next_serve() {
    let site = Site::linear(&[]);
    let stale = site.plant_split("00000000000000aa", DEAD_SPLIT_AGE);
    let recent = site.plant_split("00000000000000bb", Duration::ZERO);
    site.clone("one");
    site.records(1);
    assert!(!stale.exists(), "the stale split stayed");
    assert!(recent.exists(), "the recent split was removed");
}
