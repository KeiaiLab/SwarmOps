//! The git transport (M13): the user's git spawned against a mirror, its environment, its
//! credentials, the URLs a remote may have, and the classification of git's failures, against the
//! test kit's loopback server and ssh stand-in. Written by the M13 plan's task C3.
//!
//! Each test builds its [`GitCli`] from an environment of its own
//! ([`GitCli::find_with_env`]), so that no test changes the process's environment. A "stand-in"
//! is a shell script named in `SWARMOPS_GIT` that writes down how it was started (its arguments,
//! its environment, what its stdin is and holds) and then runs the real git.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use swarmops_git::oid::GitOid;
use swarmops_git::transport::{
    Cause, Credential, GitCli, PushOutcome, PushUpdate, RefSpec, RejectReason, RemoteUrl,
    Transport, TransportError, UrlRefused,
};
use swarmops_testkit::remote::{GitServer, HttpAuth, SshMode};
use tempfile::TempDir;

/// The repository every test serves.
const REPO: &str = "repo";

/// The variable a remote names for its token.
const TOKEN_VAR: &str = "SWARMOPS_TEST_REMOTE_TOKEN";

/// The token the tests' server asks for.
const TOKEN: &str = "ghs_transportTestToken0123456789";

/// The user name a token goes with (GitHub's, for an installation token).
const TOKEN_USER: &str = "x-access-token";

/// What a stand-in that is not the real git prints for `git version`, below the floor.
const OLD_VERSION: &str = "2.38.5";

/// A served repository with one commit on `main`, an empty mirror, and the environment a
/// [`GitCli`] is built from.
struct Setup {
    /// The remote's side.
    server: GitServer,
    /// The mirror, `home/` (git's empty `HOME`) and the stand-ins' files.
    dir: TempDir,
    /// The commit on the remote's `main`.
    first: GitOid,
}

impl Setup {
    /// The served repository, its first commit, and the mirror.
    fn new() -> Setup {
        let server = GitServer::new();
        server.create(REPO);
        let first = server.commit_on(REPO, "main", "first", &[("a.txt", "a\n")]);
        let dir = tempfile::tempdir().expect("a temp dir");

        // The mirror: a bare repository, made by plain git.
        fs::create_dir(dir.path().join("home")).expect("create HOME");
        let ok = server
            .command()
            .args(["init", "--quiet", "--bare"])
            .arg(dir.path().join("mirror.git"))
            .status()
            .expect("run git init");
        assert!(ok.success(), "git init of the mirror");
        Setup {
            server,
            dir,
            first: oid(&first),
        }
    }

    /// The mirror's directory.
    fn mirror(&self) -> PathBuf {
        self.dir.path().join("mirror.git")
    }

    /// The real git, as an absolute path.
    fn git(&self) -> PathBuf {
        real_git(&self.server)
    }

    /// The environment of a caller whose git is `git`, plus `extra`: the caller's `PATH`, an
    /// empty `HOME`, no system or global configuration.
    fn env(&self, git: &Path, extra: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        let mut env: Vec<(OsString, OsString)> = vec![
            ("PATH".into(), std::env::var_os("PATH").unwrap_or_default()),
            ("HOME".into(), self.dir.path().join("home").into()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
            ("SWARMOPS_GIT".into(), git.into()),
        ];
        env.extend(
            extra
                .iter()
                .map(|(key, value)| ((*key).into(), (*value).into())),
        );
        env
    }

    /// A transport running the real git, with `extra` in its environment.
    fn cli(&self, extra: &[(&str, &str)]) -> GitCli {
        GitCli::find_with_env(self.env(&self.git(), extra)).expect("find git")
    }

    /// A transport running the stand-in that writes down each start under `dumps/` and runs the
    /// real git, with `extra` in its environment.
    fn recorded(&self, extra: &[(&str, &str)]) -> GitCli {
        let dumps = self.dir.path().join("dumps");
        let script = format!(
            "#!/bin/sh\n\
             d={dumps}/$(date +%s%N)-$$\n\
             mkdir -p \"$d\"\n\
             printf '%s\\n' \"$@\" > \"$d/argv\"\n\
             env > \"$d/env\"\n\
             readlink /proc/$$/fd/0 > \"$d/stdin-kind\"\n\
             cat > \"$d/stdin\"\n\
             exec {git} \"$@\" < \"$d/stdin\"\n",
            dumps = sh_quote(&dumps),
            git = sh_quote(&self.git()),
        );
        let program = self.script("recorded", &script);
        GitCli::find_with_env(self.env(&program, extra)).expect("find the stand-in")
    }

    /// A transport running a stand-in that answers `git version` as the real git does and
    /// fails every other command by running `body`.
    fn failing(&self, name: &str, body: &str, extra: &[(&str, &str)]) -> GitCli {
        let script = format!(
            "#!/bin/sh\n\
             if [ \"$1\" = version ]; then exec {git} version; fi\n\
             {body}\n",
            git = sh_quote(&self.git()),
        );
        let program = self.script(name, &script);
        GitCli::find_with_env(self.env(&program, extra)).expect("find the stand-in")
    }

    /// Writes the executable script `<dir>/<name>/git` and returns its path.
    fn script(&self, name: &str, text: &str) -> PathBuf {
        let dir = self.dir.path().join(name);
        fs::create_dir_all(&dir).expect("create the stand-in's directory");
        let program = dir.join("git");
        fs::write(&program, text).expect("write the stand-in");
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).expect("chmod");
        program
    }

    /// Every start the recording stand-in wrote down, in order.
    fn dumps(&self) -> Vec<Dump> {
        let Ok(entries) = fs::read_dir(self.dir.path().join("dumps")) else {
            return Vec::new();
        };
        let mut dirs: Vec<PathBuf> = entries.map(|entry| entry.expect("a dump").path()).collect();
        dirs.sort();
        dirs.iter().map(|dir| Dump::read(dir)).collect()
    }

    /// Fetches the remote's branches into the mirror's `refs/remotes/origin/*`.
    fn fetch(&self, cli: &GitCli, url: &str, cred: &Credential) -> Result<(), TransportError> {
        let url = RemoteUrl::parse(url).expect("a test URL");
        cli.fetch(&self.mirror(), &url, cred, &[branches()])
    }

    /// A commit in the mirror on `parent` (a root commit when `None`) that writes `files` over
    /// its tree.
    fn commit(&self, parent: Option<GitOid>, message: &str, files: &[(&str, &[u8])]) -> GitOid {
        let index = self.dir.path().join("scratch-index");
        let _ = fs::remove_file(&index);

        // 1. The parent's tree, in a scratch index.
        let mut read = self.in_mirror();
        read.env("GIT_INDEX_FILE", &index).arg("read-tree");
        match parent {
            Some(parent) => read.arg(parent.to_string()),
            None => read.arg("--empty"),
        };
        run(&mut read);

        // 2. Each file's blob entered.
        for (path, bytes) in files {
            let file = self.dir.path().join("scratch-blob");
            fs::write(&file, bytes).expect("write a blob");
            let blob = run(self.in_mirror().args(["hash-object", "-w"]).arg(&file));
            run(self
                .in_mirror()
                .env("GIT_INDEX_FILE", &index)
                .args(["update-index", "--add", "--cacheinfo"])
                .arg(format!("100644,{blob},{path}")));
        }

        // 3. The tree, committed.
        let tree = run(self
            .in_mirror()
            .env("GIT_INDEX_FILE", &index)
            .arg("write-tree"));
        let mut commit = self.in_mirror();
        commit.args(["commit-tree", &tree, "-m", message]);
        if let Some(parent) = parent {
            commit.args(["-p", &parent.to_string()]);
        }
        oid(&run(&mut commit))
    }

    /// A plain git command on the mirror.
    fn in_mirror(&self) -> Command {
        let mut command = self.server.command();
        command.env("GIT_DIR", self.mirror());
        command
    }

    /// What the remote's `branch` names.
    fn remote(&self, branch: &str) -> Option<GitOid> {
        self.server
            .rev_parse(REPO, &format!("refs/heads/{branch}"))
            .map(|hex| oid(&hex))
    }
}

/// One start of the recording stand-in.
struct Dump {
    /// Its arguments.
    argv: Vec<String>,
    /// Its environment, `KEY=value` lines.
    env: Vec<String>,
    /// What its stdin was: `/dev/null`, or `pipe:[…]`.
    stdin_kind: String,
    /// What its stdin held.
    stdin: String,
}

impl Dump {
    /// The start written down in `dir`.
    fn read(dir: &Path) -> Dump {
        let read = |name: &str| fs::read_to_string(dir.join(name)).expect("read a dump file");
        Dump {
            argv: read("argv").lines().map(str::to_owned).collect(),
            env: read("env").lines().map(str::to_owned).collect(),
            stdin_kind: read("stdin-kind").trim_end().to_owned(),
            stdin: read("stdin"),
        }
    }

    /// The git subcommand it ran (its first argument).
    fn subcommand(&self) -> &str {
        self.argv.first().map_or("", String::as_str)
    }

    /// The value of `key` in its environment.
    fn var(&self, key: &str) -> Option<&str> {
        self.env
            .iter()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
    }

    /// The configuration it got through `GIT_CONFIG_COUNT`, as `(key, value)` pairs.
    fn config(&self) -> Vec<(String, String)> {
        let count: usize = self
            .var("GIT_CONFIG_COUNT")
            .map_or(0, |count| count.parse().expect("a count"));
        (0..count)
            .map(|n| {
                let key = self.var(&format!("GIT_CONFIG_KEY_{n}")).expect("a key");
                let value = self.var(&format!("GIT_CONFIG_VALUE_{n}")).expect("a value");
                (key.to_owned(), value.to_owned())
            })
            .collect()
    }
}

/// The real git the server runs, as an absolute path.
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

/// The fetch refspec of every branch, into `refs/remotes/origin/*`.
fn branches() -> RefSpec {
    RefSpec {
        src: "refs/heads/*".to_owned(),
        dst: "refs/remotes/origin/*".to_owned(),
    }
}

/// `hex` as an oid.
fn oid(hex: &str) -> GitOid {
    hex.trim().parse().expect("an oid")
}

/// Runs `command`, which must succeed, and returns its stdout, trimmed.
fn run(command: &mut Command) -> String {
    let output = command.stdin(Stdio::null()).output().expect("run git");
    assert!(
        output.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// `path` quoted for `sh`.
fn sh_quote(path: &Path) -> String {
    let text = path.to_str().expect("a UTF-8 path");
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The `Authorization` header a token is sent as.
fn basic(token: &str) -> String {
    use base64::Engine as _;
    let pair = format!("{TOKEN_USER}:{token}");
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(pair)
    )
}

/// The credential that names [`TOKEN_VAR`].
fn token_cred() -> Credential {
    Credential {
        token_env: Some(TOKEN_VAR.to_owned()),
    }
}

/// The server's Basic authentication with [`TOKEN`].
fn token_auth() -> HttpAuth {
    HttpAuth::Basic {
        user: TOKEN_USER.to_owned(),
        token: TOKEN.to_owned(),
    }
}

/// The reason of a refusal, or a panic naming what came instead.
fn rejected(
    result: Result<impl std::fmt::Debug, TransportError>,
) -> (RejectReason, TransportError) {
    match result {
        Err(error @ TransportError::Rejected { reason, .. }) => (reason, error),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// The cause of an unreachable remote, or a panic naming what came instead.
fn unreachable(result: Result<impl std::fmt::Debug, TransportError>) -> Cause {
    match result {
        Err(TransportError::Unreachable { cause, .. }) => cause,
        other => panic!("expected an unreachable remote, got {other:?}"),
    }
}

// The classifier, one test per row of the brief's §3.5.

#[test]
fn stale_lease_is_stale() {
    let setup = Setup::new();
    let http = setup.server.http(HttpAuth::Anonymous);
    let cli = setup.cli(&[]);
    setup
        .fetch(&cli, &http.url(REPO), &Credential::default())
        .expect("fetch");

    // Another writer moves main after the fetch; the push expects what the fetch saw.
    let moved = setup
        .server
        .commit_on(REPO, "main", "theirs", &[("b.txt", "b\n")]);
    let ours = setup.commit(Some(setup.first), "ours", &[("c.txt", b"c\n")]);
    let url = RemoteUrl::parse(&http.url(REPO)).expect("a URL");
    let update = PushUpdate {
        new: ours,
        branch: "main".to_owned(),
        expect: Some(setup.first),
    };
    let result = cli.push(&setup.mirror(), &url, &Credential::default(), &update);
    assert_eq!(
        result,
        Err(TransportError::Stale {
            branch: "main".to_owned()
        })
    );
    assert_eq!(
        setup.remote("main"),
        Some(oid(&moved)),
        "the remote kept its head"
    );
}

#[test]
fn up_to_date_flag_is_success() {
    let setup = Setup::new();
    let http = setup.server.http(HttpAuth::Anonymous);
    let cli = setup.cli(&[]);
    let cred = Credential::default();
    setup.fetch(&cli, &http.url(REPO), &cred).expect("fetch");
    let url = RemoteUrl::parse(&http.url(REPO)).expect("a URL");

    // 1. The remote holds the commit already: git's `=`, nothing sent.
    let same = PushUpdate {
        new: setup.first,
        branch: "main".to_owned(),
        expect: Some(setup.first),
    };
    assert_eq!(
        cli.push(&setup.mirror(), &url, &cred, &same),
        Ok(PushOutcome::AlreadyThere)
    );

    // 2. A fast-forward moves it.
    let next = setup.commit(Some(setup.first), "next", &[("c.txt", b"c\n")]);
    let forward = PushUpdate {
        new: next,
        branch: "main".to_owned(),
        expect: Some(setup.first),
    };
    assert_eq!(
        cli.push(&setup.mirror(), &url, &cred, &forward),
        Ok(PushOutcome::Sent)
    );
    assert_eq!(setup.remote("main"), Some(next));

    // 3. A branch the remote lacks is made, expected absent.
    let new_branch = PushUpdate {
        new: next,
        branch: "topic".to_owned(),
        expect: None,
    };
    assert_eq!(
        cli.push(&setup.mirror(), &url, &cred, &new_branch),
        Ok(PushOutcome::Sent)
    );
    assert_eq!(setup.remote("topic"), Some(next));

    // 4. Expected absent but present: the lease fails.
    let again = PushUpdate {
        new: setup.first,
        branch: "topic".to_owned(),
        expect: None,
    };
    assert_eq!(
        cli.push(&setup.mirror(), &url, &cred, &again),
        Err(TransportError::Stale {
            branch: "topic".to_owned()
        })
    );
}

#[test]
fn declined_push_is_rejected_with_the_message() {
    let setup = Setup::new();
    let http = setup.server.http(HttpAuth::Anonymous);
    let cli = setup.cli(&[]);
    let cred = Credential::default();
    setup.fetch(&cli, &http.url(REPO), &cred).expect("fetch");
    setup
        .server
        .decline(REPO, "main", "protected branch: open a pull request");

    // The hook declines: the ref, git's reason and the remote's own line are reported.
    let next = setup.commit(Some(setup.first), "next", &[("c.txt", b"c\n")]);
    let url = RemoteUrl::parse(&http.url(REPO)).expect("a URL");
    let update = PushUpdate {
        new: next,
        branch: "main".to_owned(),
        expect: Some(setup.first),
    };
    let (reason, error) = rejected(cli.push(&setup.mirror(), &url, &cred, &update));
    assert_eq!(reason, RejectReason::Declined);
    let TransportError::Rejected {
        refs,
        remote_messages,
        git_exit,
        ..
    } = error
    else {
        unreachable!()
    };
    assert_eq!(
        refs,
        vec![(
            "refs/heads/main".to_owned(),
            "pre-receive hook declined".to_owned()
        )]
    );
    assert!(
        remote_messages
            .iter()
            .any(|line| line == "protected branch: open a pull request"),
        "{remote_messages:?}"
    );
    assert_ne!(git_exit, Some(0));
    assert_eq!(setup.remote("main"), Some(setup.first));
}

#[test]
fn http_401_is_auth() {
    let setup = Setup::new();
    let http = setup.server.http(token_auth());

    // 1. No credential at all: git may not prompt, so it fails as auth.
    let cli = setup.cli(&[]);
    let (reason, error) = rejected(setup.fetch(&cli, &http.url(REPO), &Credential::default()));
    assert_eq!(reason, RejectReason::Auth);
    assert!(
        matches!(
            error,
            TransportError::Rejected {
                token_env_unset: false,
                ..
            }
        ),
        "{error:?}"
    );

    // 2. The remote names a token variable that is unset: the same, and said.
    let (reason, error) = rejected(setup.fetch(&cli, &http.url(REPO), &token_cred()));
    assert_eq!(reason, RejectReason::Auth);
    assert!(
        matches!(
            error,
            TransportError::Rejected {
                token_env_unset: true,
                ..
            }
        ),
        "{error:?}"
    );

    // 3. A wrong token: refused.
    let wrong = setup.cli(&[(TOKEN_VAR, "ghs_wrong")]);
    let (reason, _) = rejected(setup.fetch(&wrong, &http.url(REPO), &token_cred()));
    assert_eq!(reason, RejectReason::Auth);

    // 4. The right one: served.
    let right = setup.cli(&[(TOKEN_VAR, TOKEN)]);
    setup
        .fetch(&right, &http.url(REPO), &token_cred())
        .expect("an authenticated fetch");
}

#[test]
fn ssh_permission_denied_is_auth() {
    let setup = Setup::new();
    let ssh = setup.server.ssh_command(SshMode::Deny);
    let cli = setup.cli(&[("GIT_SSH_COMMAND", &ssh)]);
    let (reason, _) =
        rejected(setup.fetch(&cli, &setup.server.ssh_url(REPO), &Credential::default()));
    assert_eq!(reason, RejectReason::Auth);
}

#[test]
fn missing_repository_is_not_found() {
    let setup = Setup::new();
    let cred = Credential::default();

    // 1. Over HTTP: a 404.
    let http = setup.server.http(HttpAuth::Anonymous);
    let cli = setup.cli(&[]);
    let (reason, _) = rejected(setup.fetch(&cli, &http.url("nope"), &cred));
    assert_eq!(reason, RejectReason::NotFound);

    // 2. Over ssh: no repository at the path.
    let ssh = setup.server.ssh_command(SshMode::Serve);
    let cli = setup.cli(&[("GIT_SSH_COMMAND", &ssh)]);
    let (reason, _) = rejected(setup.fetch(&cli, &setup.server.ssh_url("nope"), &cred));
    assert_eq!(reason, RejectReason::NotFound);

    // 3. The same remote's repository over ssh is served.
    setup
        .fetch(&cli, &setup.server.ssh_url(REPO), &cred)
        .expect("an ssh fetch");
}

#[test]
fn refused_connection_is_unreachable() {
    let setup = Setup::new();
    let cred = Credential::default();

    // 1. Over HTTP: nothing listens.
    let mut http = setup.server.http(HttpAuth::Anonymous);
    let url = http.url(REPO);
    http.stop();
    let cli = setup.cli(&[]);
    assert_eq!(unreachable(setup.fetch(&cli, &url, &cred)), Cause::Connect);

    // 2. Over ssh: ssh cannot reach the host.
    let ssh = setup.server.ssh_command(SshMode::Down);
    let cli = setup.cli(&[("GIT_SSH_COMMAND", &ssh)]);
    assert_eq!(
        unreachable(setup.fetch(&cli, &setup.server.ssh_url(REPO), &cred)),
        Cause::Ssh
    );
}

#[test]
fn cut_transfer_is_unreachable() {
    let setup = Setup::new();
    let big = (0..20_000).fold(String::new(), |mut big, n| {
        let _ = writeln!(big, "line {n}");
        big
    });
    setup
        .server
        .commit_on(REPO, "main", "big", &[("big.txt", &big)]);
    let http = setup.server.http(HttpAuth::Anonymous);
    let cli = setup.cli(&[]);

    // The pack's response ends early.
    http.cut_after(4096);
    let cause = unreachable(setup.fetch(&cli, &http.url(REPO), &Credential::default()));
    assert_eq!(cause, Cause::Disconnected);
}

#[test]
fn unknown_failure_is_unclassified_and_not_retryable() {
    let setup = Setup::new();
    let cli = setup.failing(
        "odd",
        "echo 'fatal: the flux capacitor is out of alignment' >&2\nexit 7",
        &[],
    );
    let http = setup.server.http(HttpAuth::Anonymous);
    let (reason, error) = rejected(setup.fetch(&cli, &http.url(REPO), &Credential::default()));
    assert_eq!(reason, RejectReason::Unclassified);
    let TransportError::Rejected {
        git_exit,
        stderr_tail,
        ..
    } = error
    else {
        unreachable!()
    };
    assert_eq!(git_exit, Some(7));
    assert!(stderr_tail.contains("flux capacitor"), "{stderr_tail}");
}

#[test]
fn sha256_remote_is_unsupported() {
    let setup = Setup::new();
    let server = GitServer::new_sha256();
    server.create(REPO);
    server.commit_on(REPO, "main", "first", &[("a.txt", "a\n")]);
    let http = server.http(HttpAuth::Anonymous);
    let cli = setup.cli(&[]);
    assert_eq!(
        setup.fetch(&cli, &http.url(REPO), &Credential::default()),
        Err(TransportError::Sha256)
    );
}

// How every child is started.

#[test]
fn child_env_drops_git_dir_and_traces() {
    let setup = Setup::new();
    let http = setup.server.http(HttpAuth::Anonymous);
    let cli = setup.recorded(&[
        ("GIT_DIR", "/elsewhere.git"),
        ("GIT_WORK_TREE", "/elsewhere"),
        ("GIT_TRACE", "1"),
        ("GIT_TRACE_CURL", "1"),
        ("GIT_CURL_VERBOSE", "1"),
        ("GIT_CONFIG_PARAMETERS", "'core.hookspath'='/elsewhere'"),
        ("LANG", "de_DE.UTF-8"),
        ("LC_MESSAGES", "de_DE.UTF-8"),
        ("GIT_SSH_COMMAND", "ssh -i /keys/id"),
    ]);
    setup
        .fetch(&cli, &http.url(REPO), &Credential::default())
        .expect("fetch");
    cli.gc_auto(&setup.mirror()).expect("gc");

    // Every child: the caller's repository and traces gone, the rules' own set.
    let dumps = setup.dumps();
    assert!(dumps.len() >= 3, "version, fetch and gc ran");
    let mirror = setup.mirror();
    for dump in &dumps {
        for gone in [
            "GIT_WORK_TREE",
            "GIT_TRACE",
            "GIT_TRACE_CURL",
            "GIT_CURL_VERBOSE",
            "GIT_CONFIG_PARAMETERS",
            "LANG",
            "LC_MESSAGES",
        ] {
            assert_eq!(dump.var(gone), None, "{gone} reached {:?}", dump.argv);
        }
        assert_ne!(dump.var("GIT_DIR"), Some("/elsewhere.git"));
        assert_eq!(dump.var("LC_ALL"), Some("C"));
        assert_eq!(dump.var("GIT_SSH_COMMAND"), Some("ssh -i /keys/id"));
        assert!(dump.var("HOME").is_some());
        assert!(
            !dump
                .argv
                .iter()
                .any(|arg| arg == "-c" || arg.starts_with("--config")),
            "configuration in the arguments: {:?}",
            dump.argv
        );
    }

    // The mirror's commands: GIT_DIR is the mirror, and the configuration is the rules'.
    for dump in dumps.iter().filter(|dump| dump.subcommand() != "version") {
        assert_eq!(dump.var("GIT_DIR"), mirror.to_str(), "{:?}", dump.argv);
        assert_eq!(dump.var("GIT_TERMINAL_PROMPT"), Some("0"));
        let config = dump.config();
        for (key, value) in [
            ("core.fsync", "all"),
            ("core.fsyncMethod", "fsync"),
            ("gc.autoDetach", "false"),
            ("protocol.version", "2"),
            ("fetch.recurseSubmodules", "false"),
            ("transfer.credentialsInUrl", "die"),
        ] {
            assert!(
                config.contains(&(key.to_owned(), value.to_owned())),
                "{key}={value} missing from {config:?}"
            );
        }
        let hooks = mirror.join("hooks-none");
        assert!(
            config.contains(&(
                "core.hooksPath".to_owned(),
                hooks.to_str().unwrap().to_owned()
            )),
            "{config:?}"
        );
    }

    // The fetch: http only, and the arguments end in --end-of-options, the URL and the refspec.
    let fetch = dumps
        .iter()
        .find(|dump| dump.subcommand() == "fetch")
        .expect("a fetch");
    assert_eq!(fetch.var("GIT_ALLOW_PROTOCOL"), Some("http"));
    let end = fetch
        .argv
        .iter()
        .position(|arg| arg == "--end-of-options")
        .expect("--end-of-options");
    assert_eq!(
        fetch.argv[end + 1..],
        [
            http.url(REPO),
            "+refs/heads/*:refs/remotes/origin/*".to_owned()
        ]
    );
    for flag in [
        "--no-tags",
        "--prune",
        "--no-write-fetch-head",
        "--no-auto-gc",
        "--no-recurse-submodules",
        "--no-show-forced-updates",
    ] {
        assert!(fetch.argv[..end].iter().any(|arg| arg == flag), "{flag}");
    }
}

#[test]
fn token_is_never_in_arguments_or_output() {
    let setup = Setup::new();
    let http = setup.server.http(token_auth());
    let header = basic(TOKEN);
    let encoded = header.trim_start_matches("Basic ").to_owned();

    // 1. An authenticated fetch and push: no child's arguments hold the token, in any form.
    let cli = setup.recorded(&[(TOKEN_VAR, TOKEN)]);
    setup
        .fetch(&cli, &http.url(REPO), &token_cred())
        .expect("fetch");
    let next = setup.commit(Some(setup.first), "next", &[("c.txt", b"c\n")]);
    let url = RemoteUrl::parse(&http.url(REPO)).expect("a URL");
    let update = PushUpdate {
        new: next,
        branch: "main".to_owned(),
        expect: Some(setup.first),
    };
    assert_eq!(
        cli.push(&setup.mirror(), &url, &token_cred(), &update),
        Ok(PushOutcome::Sent)
    );
    let dumps = setup.dumps();
    assert!(dumps.iter().any(|dump| dump.subcommand() == "push"));
    for dump in &dumps {
        let argv = dump.argv.join("\n");
        assert!(!argv.contains(TOKEN), "{argv}");
        assert!(!argv.contains(&encoded), "{argv}");
    }

    // 2. A git that prints everything it was given: the error holds none of it.
    let leaky = setup.failing(
        "leaky",
        &format!("env >&2\necho \"$@\" >&2\necho 'Authorization: {header}' >&2\nexit 1"),
        &[(TOKEN_VAR, TOKEN)],
    );
    let error = setup
        .fetch(&leaky, &http.url(REPO), &token_cred())
        .expect_err("the stand-in fails");
    let text = format!("{error:?}");
    assert!(!text.contains(TOKEN), "{text}");
    assert!(!text.contains(&encoded), "{text}");
    assert!(text.contains("<redacted>"), "{text}");
}

#[test]
fn mirror_path_is_never_in_output() {
    let setup = Setup::new();
    let http = setup.server.http(HttpAuth::Anonymous);
    let cli = setup.cli(&[]);

    // 1. A held ref lock: git names the lock file, under the mirror, in its stderr.
    let refs = setup.mirror().join("refs/remotes/origin");
    fs::create_dir_all(&refs).expect("the refs directory");
    fs::write(refs.join("main.lock"), "").expect("the lock");
    let error = setup
        .fetch(&cli, &http.url(REPO), &Credential::default())
        .expect_err("the lock fails the fetch");

    // 2. The error holds the mirror's path in no form, given or canonical.
    let text = format!("{error:?}");
    let given = setup.mirror().display().to_string();
    let canonical = fs::canonicalize(setup.mirror())
        .expect("the mirror")
        .display()
        .to_string();
    assert!(text.contains("main.lock"), "{text}");
    assert!(!text.contains(&given), "{text}");
    assert!(!text.contains(&canonical), "{text}");
}

#[test]
fn token_header_is_scoped_to_the_remote_url() {
    let setup = Setup::new();
    let http = setup.server.http(token_auth());

    // 1. The token reaches the server as the header, with no prompt.
    let cli = setup.recorded(&[(TOKEN_VAR, TOKEN)]);
    setup
        .fetch(&cli, &http.url(REPO), &token_cred())
        .expect("an authenticated fetch");
    assert!(
        http.authorizations().contains(&basic(TOKEN)),
        "{:?}",
        http.authorizations()
    );

    // 2. The header's key names the remote's URL, and no key sets it for every URL.
    let fetch = setup
        .dumps()
        .into_iter()
        .find(|dump| dump.subcommand() == "fetch")
        .expect("a fetch");
    let config = fetch.config();
    let scoped = format!("http.{}.extraHeader", http.url(REPO));
    let headers: Vec<&(String, String)> = config
        .iter()
        .filter(|(key, _)| key.to_ascii_lowercase().ends_with(".extraheader"))
        .collect();
    assert_eq!(
        headers,
        vec![&(scoped, format!("Authorization: {}", basic(TOKEN)))]
    );

    // 3. git, given that configuration, sends nothing to another server.
    let other = GitServer::new();
    other.create(REPO);
    other.commit_on(REPO, "main", "first", &[("a.txt", "a\n")]);
    let other_http = other.http(HttpAuth::Anonymous);
    let mut plain = setup.server.command();
    plain.env("GIT_CONFIG_COUNT", config.len().to_string());
    for (n, (key, value)) in config.iter().enumerate() {
        plain
            .env(format!("GIT_CONFIG_KEY_{n}"), key)
            .env(format!("GIT_CONFIG_VALUE_{n}"), value);
    }
    run(plain.args(["ls-remote", &other_http.url(REPO)]));
    assert_eq!(other_http.authorizations(), Vec::<String>::new());
}

#[test]
fn url_rules() {
    // 1. Accepted, with the host each names.
    for (url, host) in [
        ("https://github.com/KeiaiLab/SwarmOps.git", "github.com"),
        ("https://github.com:443/a/b", "github.com"),
        ("https://user@github.com/a/b.git", "github.com"),
        ("https://github.com", "github.com"),
        ("ssh://git@github.com/a/b.git", "github.com"),
        ("ssh://git@ssh.github.com:443/a/b.git", "ssh.github.com"),
        ("ssh://host.example/srv/repo.git", "host.example"),
        ("git@github.com:a/b.git", "github.com"),
        ("host.example:srv/repo.git", "host.example"),
        ("http://127.0.0.1:8080/a.git", "127.0.0.1"),
        ("http://127.1.2.3/a.git", "127.1.2.3"),
        ("http://[::1]:9418/a.git", "::1"),
    ] {
        let parsed = RemoteUrl::parse(url).unwrap_or_else(|why| panic!("{url}: {why:?}"));
        assert_eq!(parsed.as_str(), url, "kept as given");
        assert_eq!(parsed.host(), host, "{url}");
    }

    // 2. Refused, each for its reason.
    for (url, why) in [
        ("http://example.com/a.git", UrlRefused::CleartextHttp),
        ("http://localhost:8080/a.git", UrlRefused::CleartextHttp),
        ("http://10.0.0.1/a.git", UrlRefused::CleartextHttp),
        ("http://[::2]/a.git", UrlRefused::CleartextHttp),
        ("file:///srv/repo.git", UrlRefused::Scheme),
        ("/srv/repo.git", UrlRefused::Scheme),
        ("./repo.git", UrlRefused::Scheme),
        ("repo.git", UrlRefused::Scheme),
        ("", UrlRefused::Scheme),
        ("ext::sh -c touch% /tmp/pwned", UrlRefused::Scheme),
        ("fd::17", UrlRefused::Scheme),
        ("git://github.com/a/b.git", UrlRefused::Scheme),
        ("ftp://host.example/a.git", UrlRefused::Scheme),
        ("https:///a.git", UrlRefused::Scheme),
        ("https://github.com/a\nb", UrlRefused::Scheme),
        (
            "https://user:pass@github.com/a.git",
            UrlRefused::CredentialInUrl,
        ),
        (
            "https://user:@github.com/a.git",
            UrlRefused::CredentialInUrl,
        ),
        (
            "ssh://git:pass@github.com/a.git",
            UrlRefused::CredentialInUrl,
        ),
        (
            "http://user:pass@127.0.0.1/a.git",
            UrlRefused::CredentialInUrl,
        ),
        ("--upload-pack=touch /tmp/pwned", UrlRefused::OptionLike),
        ("-oProxyCommand=touch /tmp/pwned", UrlRefused::OptionLike),
        ("ssh://-oProxyCommand=sh/a.git", UrlRefused::OptionLike),
        ("ssh://-user@host.example/a.git", UrlRefused::OptionLike),
        ("https://-host.example/a.git", UrlRefused::OptionLike),
        ("git@-host.example:a.git", UrlRefused::OptionLike),
    ] {
        assert_eq!(RemoteUrl::parse(url), Err(why), "{url:?}");
    }
}

#[test]
fn git_too_old_and_missing() {
    let setup = Setup::new();

    // 1. No git: SWARMOPS_GIT unset and none on PATH; and SWARMOPS_GIT naming nothing.
    let empty = setup.dir.path().join("empty-path");
    fs::create_dir(&empty).expect("an empty dir");
    let none: Vec<(OsString, OsString)> = vec![("PATH".into(), empty.clone().into())];
    assert!(matches!(
        GitCli::find_with_env(none),
        Err(TransportError::GitMissing)
    ));
    let nothing: Vec<(OsString, OsString)> = vec![
        ("PATH".into(), empty.into()),
        (
            "SWARMOPS_GIT".into(),
            setup.dir.path().join("no-git").into(),
        ),
    ];
    assert!(matches!(
        GitCli::find_with_env(nothing),
        Err(TransportError::GitMissing)
    ));

    // 2. A git below 2.39: find runs nothing and succeeds; the first command refuses it.
    let log = setup.dir.path().join("old-git.log");
    let script = format!(
        "#!/bin/sh\necho \"$1\" >> {log}\n\
         if [ \"$1\" = version ]; then echo 'git version {OLD_VERSION}'; exit 0; fi\nexit 0\n",
        log = sh_quote(&log),
    );
    let program = setup.script("old", &script);
    let cli = GitCli::find_with_env(setup.env(&program, &[])).expect("find runs nothing");
    assert!(!log.exists(), "find ran git");
    assert_eq!(
        cli.gc_auto(&setup.mirror()),
        Err(TransportError::GitTooOld {
            found: OLD_VERSION.to_owned()
        })
    );
    assert_eq!(
        fs::read_to_string(&log).expect("the log"),
        "version\n",
        "only the version was read"
    );

    // 3. git found on PATH when SWARMOPS_GIT is unset.
    let dir = setup.git().parent().expect("git's dir").to_path_buf();
    let on_path: Vec<(OsString, OsString)> = vec![("PATH".into(), dir.into())];
    let found = GitCli::find_with_env(on_path).expect("git on PATH");
    assert_eq!(found.binary(), setup.git());
}

#[test]
fn push_never_has_a_plus_or_force() {
    let setup = Setup::new();
    let http = setup.server.http(HttpAuth::Anonymous);
    let cli = setup.recorded(&[]);
    let cred = Credential::default();
    setup.fetch(&cli, &http.url(REPO), &cred).expect("fetch");
    let url = RemoteUrl::parse(&http.url(REPO)).expect("a URL");
    let next = setup.commit(Some(setup.first), "next", &[("c.txt", b"c\n")]);

    // Every kind of push the module builds: expected present, expected absent, a stale lease.
    for update in [
        PushUpdate {
            new: next,
            branch: "main".to_owned(),
            expect: Some(setup.first),
        },
        PushUpdate {
            new: next,
            branch: "topic".to_owned(),
            expect: None,
        },
        PushUpdate {
            new: setup.first,
            branch: "main".to_owned(),
            expect: Some(setup.first),
        },
    ] {
        let _ = cli.push(&setup.mirror(), &url, &cred, &update);
    }

    // No force flag, no `+` refspec; the lease and porcelain always.
    let pushes: Vec<Dump> = setup
        .dumps()
        .into_iter()
        .filter(|dump| dump.subcommand() == "push")
        .collect();
    assert_eq!(pushes.len(), 3);
    for push in &pushes {
        for arg in &push.argv {
            assert!(
                arg != "--force" && arg != "-f" && !arg.starts_with('+'),
                "{:?}",
                push.argv
            );
        }
        for flag in [
            "--porcelain",
            "--no-verify",
            "--no-follow-tags",
            "--no-signed",
        ] {
            assert!(push.argv.iter().any(|arg| arg == flag), "{flag}");
        }
        assert!(
            push.argv
                .iter()
                .any(|arg| arg.starts_with("--force-with-lease=refs/heads/")),
            "{:?}",
            push.argv
        );
        let end = push
            .argv
            .iter()
            .position(|arg| arg == "--end-of-options")
            .expect("--end-of-options");
        assert_eq!(push.argv.len(), end + 3, "the URL and one refspec follow");
        assert_eq!(push.argv[end + 1], http.url(REPO));
    }
}

#[test]
fn is_ancestor_reads_the_mirror() {
    let setup = Setup::new();
    let cli = setup.cli(&[]);
    let mirror = setup.mirror();
    let base = setup.commit(None, "base", &[("a.txt", b"a\n")]);
    let child = setup.commit(Some(base), "child", &[("b.txt", b"b\n")]);
    let sibling = setup.commit(Some(base), "sibling", &[("c.txt", b"c\n")]);
    let unrelated = setup.commit(None, "unrelated", &[("d.txt", b"d\n")]);

    assert_eq!(cli.is_ancestor(&mirror, base, child), Ok(true));
    assert_eq!(cli.is_ancestor(&mirror, child, child), Ok(true));
    assert_eq!(cli.is_ancestor(&mirror, child, base), Ok(false));
    assert_eq!(cli.is_ancestor(&mirror, sibling, child), Ok(false));
    assert_eq!(cli.is_ancestor(&mirror, unrelated, child), Ok(false));

    // A commit the mirror lacks is an error, not an answer.
    let absent = oid("0123456789abcdef0123456789abcdef01234567");
    assert!(cli.is_ancestor(&mirror, absent, child).is_err());
}

#[test]
fn new_objects_lists_blobs_and_disk_usage() {
    let setup = Setup::new();
    let cli = setup.cli(&[]);
    let mirror = setup.mirror();
    let big = vec![b'x'; 1000];
    let base = setup.commit(None, "base", &[("a.txt", b"a\n")]);
    let next = setup.commit(Some(base), "next", &[("dir/big.bin", &big)]);

    // 1. Against its parent: the one new blob, at its path.
    let added = cli
        .new_objects(&mirror, next, Some(base))
        .expect("new objects");
    let blobs: Vec<(u64, &str)> = added
        .blobs
        .iter()
        .map(|(_, size, path)| (*size, path.as_str()))
        .collect();
    assert_eq!(blobs, vec![(1000, "dir/big.bin")]);
    assert!(added.disk_usage > 0);

    // 2. Against nothing: every blob.
    let all = cli.new_objects(&mirror, next, None).expect("new objects");
    let mut paths: Vec<&str> = all.blobs.iter().map(|(_, _, path)| path.as_str()).collect();
    paths.sort_unstable();
    assert_eq!(paths, vec!["a.txt", "dir/big.bin"]);
    assert!(all.disk_usage >= added.disk_usage);

    // 3. Nothing new: nothing.
    let none = cli
        .new_objects(&mirror, base, Some(next))
        .expect("new objects");
    assert_eq!(none.blobs, Vec::new());
}

#[test]
fn ls_remote_head_names_the_branch() {
    let setup = Setup::new();
    let http = setup.server.http(HttpAuth::Anonymous);
    let cli = setup.cli(&[]);
    let url = RemoteUrl::parse(&http.url(REPO)).expect("a URL");
    assert_eq!(
        cli.ls_remote_head(&url, &Credential::default()),
        Ok(Some("main".to_owned()))
    );

    // An empty repository: no branch, or the unborn one the server advertises.
    setup.server.create("empty");
    let empty = RemoteUrl::parse(&http.url("empty")).expect("a URL");
    let head = cli
        .ls_remote_head(&empty, &Credential::default())
        .expect("ls-remote");
    assert!(
        head.is_none() || head.as_deref() == Some("main"),
        "{head:?}"
    );
}

#[test]
fn fetch_moves_the_mirror_to_the_remote() {
    let setup = Setup::new();
    let http = setup.server.http(HttpAuth::Anonymous);
    let cli = setup.cli(&[]);
    let cred = Credential::default();
    setup.fetch(&cli, &http.url(REPO), &cred).expect("fetch");
    let tracked = |setup: &Setup| {
        run(setup
            .in_mirror()
            .args(["rev-parse", "refs/remotes/origin/main"]))
    };
    assert_eq!(oid(&tracked(&setup)), setup.first);

    // The remote moved by force: the mirror follows, as it records what the remote holds.
    let other = setup
        .server
        .commit_on(REPO, "side", "side", &[("s.txt", "s\n")]);
    setup.server.move_to(REPO, "main", &other);
    setup.fetch(&cli, &http.url(REPO), &cred).expect("fetch");
    assert_eq!(tracked(&setup), other);
}

#[cfg(target_os = "linux")]
#[test]
fn stdin_is_null_for_every_child_but_cat_file() {
    let setup = Setup::new();
    let http = setup.server.http(HttpAuth::Anonymous);
    let cli = setup.recorded(&[]);
    let cred = Credential::default();
    let url = RemoteUrl::parse(&http.url(REPO)).expect("a URL");
    let mirror = setup.mirror();

    // Every command the module runs.
    cli.ls_remote_head(&url, &cred).expect("ls-remote");
    setup.fetch(&cli, &http.url(REPO), &cred).expect("fetch");
    let next = setup.commit(Some(setup.first), "next", &[("c.txt", b"c\n")]);
    cli.new_objects(&mirror, next, Some(setup.first))
        .expect("new objects");
    cli.is_ancestor(&mirror, setup.first, next)
        .expect("is ancestor");
    let update = PushUpdate {
        new: next,
        branch: "main".to_owned(),
        expect: Some(setup.first),
    };
    cli.push(&mirror, &url, &cred, &update).expect("push");
    cli.gc_auto(&mirror).expect("gc");

    // /dev/null for each, but cat-file's pipe of 40-hex lines.
    let dumps = setup.dumps();
    let ran: Vec<&str> = dumps.iter().map(Dump::subcommand).collect();
    for expected in [
        "ls-remote",
        "fetch",
        "rev-list",
        "cat-file",
        "merge-base",
        "push",
        "gc",
    ] {
        assert!(ran.contains(&expected), "{expected} not in {ran:?}");
    }
    for dump in &dumps {
        if dump.subcommand() != "cat-file" {
            assert_eq!(dump.stdin_kind, "/dev/null", "{:?}", dump.argv);
            continue;
        }
        assert!(dump.stdin_kind.starts_with("pipe:"), "{}", dump.stdin_kind);
        assert_ne!(dump.stdin, "", "cat-file got no ids");
        for line in dump.stdin.lines() {
            assert!(
                line.len() == 40 && line.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "{line:?}"
            );
        }
    }
}
