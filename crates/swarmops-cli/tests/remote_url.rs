//! The user part of a remote's URL, through the binary, offline (the usable plan's U-16): an
//! `http://` or `https://` URL with any user part is refused, whatever the user looks like, and
//! a stored one is refused and masked; an ssh user is no credential and stays; the token goes
//! by `--token-env`.
//!
//! The remote is the test kit's [`GitServer`] on `127.0.0.1`, and the binary runs with a git
//! environment of its own, as in `remote_fetch.rs`.

use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use swarmops_testkit::remote::{GitServer, HttpAuth, HttpServer, SshMode};
use tempfile::TempDir;

use crate::support::{Run, Store, run};

/// The agent the commands that need an identity run as.
const AGENT: &str = "agent/worker-17";

/// The repository every test serves.
const REPO: &str = "repo";

/// The remote's name in the store.
const ORIGIN: &str = "origin";

/// The store's file of remotes.
const REMOTES_TOML: &str = "remotes.toml";

/// The reason a URL holding a credential is refused with.
const CREDENTIAL_IN_URL: &str = "credential_in_url";

/// What a stored URL's user part is shown as.
const MASK: &str = "***";

/// The users the rule before U-16 took for names, not tokens: under 20 characters, or without
/// both a letter and a digit. Each could be a token.
const PASSED_BEFORE: [&str; 6] = [
    "user",
    "ghp",
    "a1",
    "abcdefghijklmnopqrstuvwxyzabcdefghijklmn",
    "0123456789012345678901234567890123456789",
    "",
];

/// A served repository with two commits on `main`, and the git environment the binary runs
/// with: the server's git, an empty `HOME`, no system or global configuration.
struct Remote {
    /// The remote's side.
    server: GitServer,
    /// The server's smart HTTP.
    http: HttpServer,
    /// git's empty `HOME`, removed when the remote is dropped.
    _home: TempDir,
    /// The variables every run sets.
    env: Vec<(String, String)>,
}

impl Remote {
    /// [`REPO`] served over HTTP with `auth`.
    fn new(auth: HttpAuth) -> Remote {
        let server = GitServer::new();
        server.create(REPO);
        server.commit_on(REPO, "main", "first", &[("a.txt", "a\n")]);
        server.commit_on(REPO, "main", "second", &[("b.txt", "b\n")]);
        let http = server.http(auth);
        let home = tempfile::tempdir().expect("a temp HOME");
        let env = vec![
            ("SWARMOPS_GIT".to_owned(), path_text(&real_git(&server))),
            ("HOME".to_owned(), path_text(home.path())),
            ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
            ("GIT_CONFIG_GLOBAL".to_owned(), "/dev/null".to_owned()),
        ];
        Remote {
            server,
            http,
            _home: home,
            env,
        }
    }

    /// The repository's URL after `http://`: `127.0.0.1:<port>/repo.git`.
    fn rest(&self) -> String {
        let url = self.http.url(REPO);
        url.strip_prefix("http://").expect("an http URL").to_owned()
    }

    /// The variables every run sets, plus `extra`.
    fn vars<'a>(&'a self, extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
        let own = self.env.iter().map(|(k, v)| (k.as_str(), v.as_str()));
        own.chain(extra.iter().copied()).collect()
    }

    /// A fresh store whose commands run with this remote's git environment.
    fn store(&self) -> Store {
        Store::init().with_env(&self.vars(&[]))
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

/// `path` as text.
fn path_text(path: &Path) -> String {
    path.to_str().expect("UTF-8 temp paths").to_owned()
}

/// `remote add origin <url> --branch main` on `store`: no network call.
fn add(store: &Store, url: &str) -> Run {
    store.read(&["remote", "add", ORIGIN, url, "--branch", "main"])
}

/// `store`'s `remotes.toml`.
fn remotes_file(store: &Store) -> PathBuf {
    Path::new(store.path()).join(REMOTES_TOML)
}

/// Asserts that `refused` ended `E_INVALID_ARG` for the credential.
fn assert_credential_refused(refused: &Run, url: &str) {
    let error = refused.err("E_INVALID_ARG");
    assert_eq!(
        error["details"]["reason"], CREDENTIAL_IN_URL,
        "{url}: {refused}"
    );
}

/// A plain user name on an `https://` URL, and on `http://` to loopback, is refused by `remote
/// add` and by `clone` for the credential; neither writes a file of remotes or a store.
#[test]
fn any_userinfo_on_https_is_refused() {
    let remote = Remote::new(HttpAuth::Anonymous);
    let store = remote.store();
    let dir = tempfile::tempdir().expect("a temp dir");
    let into = dir.path().join("clone");
    let urls = [
        "https://user@github.com/acme/acme.git".to_owned(),
        "https://first-last-1990@host.example/a.git".to_owned(),
        format!("http://user@{}", remote.rest()),
    ];

    for url in &urls {
        // 1. `remote add`: refused, no file of remotes.
        assert_credential_refused(&add(&store, url), url);
        assert!(!remotes_file(&store).exists(), "{url}");

        // 2. `clone`: refused, nothing made.
        let line = ["--as", AGENT, "clone", url, "--into", &path_text(&into)];
        assert_credential_refused(&run(&line, &remote.vars(&[])), url);
        assert!(!into.exists(), "{url}");
    }

    // 3. No request reached the server.
    assert!(remote.http.requests().is_empty());
}

/// What the rule before took for a user name is refused too: a short token, one of letters
/// only, one of digits only, and an empty user. Nothing is stored, and the user part is in
/// neither stream.
#[test]
fn short_and_all_letter_tokens_are_refused() {
    let remote = Remote::new(HttpAuth::Anonymous);
    let store = remote.store();

    for user in PASSED_BEFORE {
        for url in [
            format!("https://{user}@host.example/a.git"),
            format!("http://{user}@{}", remote.rest()),
        ] {
            let refused = add(&store, &url);
            assert_credential_refused(&refused, &url);
            assert!(!remotes_file(&store).exists(), "{url}");

            // The user part is not said back; an empty one has nothing to say.
            let marked = format!("{user}@");
            let said = refused.stdout.contains(&marked) || refused.stderr.contains(&marked);
            assert!(user.is_empty() || !said, "{refused}");
        }
    }
}

/// ssh takes no token as its user: an ssh URL and the scp form are accepted with any user, a
/// token-shaped one too, and a clone over ssh with a user runs.
#[test]
fn ssh_user_is_still_accepted() {
    let remote = Remote::new(HttpAuth::Anonymous);

    // 1. Accepted and stored as given.
    for url in [
        "ssh://git@github.com/acme/acme.git",
        "ssh://deploy0a1b2c3d4e5f6a7b8c9d@host.example/a.git",
        "git@github.com:acme/acme.git",
        "deploy0a1b2c3d4e5f6a7b8c9d@host.example:a.git",
    ] {
        let store = remote.store();
        assert_eq!(add(&store, url).ok()["url"], url);
        let listed = store.read(&["remote", "list"]).ok();
        assert_eq!(listed["remotes"][0]["url"], url, "{listed}");
    }

    // 2. A clone over the ssh stand-in, whose URL names the user `git`.
    let dir = tempfile::tempdir().expect("a temp dir");
    let into = path_text(&dir.path().join("clone"));
    let url = remote.server.ssh_url(REPO);
    assert!(url.contains("git@"), "{url}");
    let ssh = remote.server.ssh_command(SshMode::Serve);
    let line = ["--as", AGENT, "clone", url.as_str(), "--into", &into];
    let cloned = run(&line, &remote.vars(&[("GIT_SSH_COMMAND", &ssh)])).ok();
    assert_eq!(cloned["imported"], 2, "{cloned}");
}

/// A remote stored with a user part on an HTTP(S) URL, whatever the user, is shown with the
/// user masked by `remote list`, and `fetch`, `push` and `sync` refuse it for the credential
/// until it is removed and added again; `remotes.toml` keeps what was stored.
#[test]
fn any_stored_userinfo_is_masked() {
    let remote = Remote::new(HttpAuth::Anonymous);
    let rest = remote.rest();
    let plain = format!("http://{rest}");

    for (stored, shown) in [
        (
            format!("http://user@{rest}"),
            format!("http://{MASK}@{rest}"),
        ),
        (
            format!("http://ghp@{rest}"),
            format!("http://{MASK}@{rest}"),
        ),
        (
            "https://user@host.example/a.git".to_owned(),
            format!("https://{MASK}@host.example/a.git"),
        ),
    ] {
        // 1. The fixture: a remote added without a user and fetched into main, the URL in its
        //    file then replaced.
        let store = remote.store();
        add(&store, &plain).ok();
        let fetched = store.read(&["fetch", ORIGIN]).ok();
        let head = fetched["head"]["changeset"].as_str().expect("a head");
        let line = ["ref", "set", "main", head, "--create"];
        store.run_as(AGENT, &line, &[]).ok();
        let file = remotes_file(&store);
        let text = fs::read_to_string(&file).expect("remotes.toml");
        let with_user = text.replace(&plain, &stored);
        assert_ne!(with_user, text, "{text}");
        fs::write(&file, &with_user).expect("write remotes.toml");

        // 2. `remote list` masks the user; the file is as written.
        let listed = store.read(&["remote", "list"]).ok();
        assert_eq!(listed["remotes"][0]["url"], shown.as_str(), "{stored}");
        assert_eq!(fs::read_to_string(&file).expect("remotes.toml"), with_user);

        // 3. Each command that reaches the remote refuses it.
        for args in [
            &["fetch", ORIGIN][..],
            &["push", ORIGIN, "main"],
            &["sync", ORIGIN],
        ] {
            assert_credential_refused(&store.run_as(AGENT, args, &[]), &stored);
        }

        // 4. Removed and added again without the user, it fetches.
        store.read(&["remote", "remove", ORIGIN]).ok();
        add(&store, &plain).ok();
        store.read(&["fetch", ORIGIN]).ok();
    }
}

/// The token's path is `--token-env`: a remote whose URL has no user part fetches and pushes
/// with the token of the variable it names, sent as `x-access-token`'s password on the fetch's
/// requests and on the push's. The same remote with the user in its URL is refused, though the
/// variable is named.
#[test]
fn token_env_half_is_used_for_fetch_and_push() {
    const TOKEN: &str = "ghp_secretTOKENvalue0123456789";
    const VAR: &str = "SO_TEST_URL_TOKEN";
    const USER: &str = "x-access-token";

    let auth = HttpAuth::Basic {
        user: USER.to_owned(),
        token: TOKEN.to_owned(),
    };
    let remote = Remote::new(auth);
    let store = remote.store();
    let rest = remote.rest();
    let env = [(VAR, TOKEN)];

    // 1. The user in the URL is refused, the variable named or not: no request is sent.
    let with_user = format!("http://{USER}@{rest}");
    let line = ["remote", "add", ORIGIN, &with_user, "--token-env", VAR];
    assert_credential_refused(&store.run_as(AGENT, &line, &env), &with_user);
    assert!(remote.http.requests().is_empty());

    // 2. Without it: added, fetched, main made from the fetched head.
    let plain = format!("http://{rest}");
    let line = ["remote", "add", ORIGIN, &plain, "--token-env", VAR];
    store.run_as(AGENT, &line, &env).ok();
    let fetched = store.run_as(AGENT, &["fetch", ORIGIN], &env).ok();
    let head = fetched["head"]["changeset"].as_str().expect("a head");
    let line = ["ref", "set", "main", head, "--create"];
    store.run_as(AGENT, &line, &[]).ok();

    // 3. A commit on main, pushed.
    let new = store.commit(AGENT, "local.txt", "local\n", Some(head));
    let line = ["ref", "set", "main", new.as_str(), "--expect", head];
    store.run_as(AGENT, &line, &[]).ok();
    store.run_as(AGENT, &["push", ORIGIN], &env).ok();

    // 4. The fetch's requests and the push's carried the token.
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{USER}:{TOKEN}"));
    let header = format!("Basic {encoded}");
    let requests = remote.http.requests();
    for service in ["git-upload-pack", "git-receive-pack"] {
        let sent = requests.iter().any(|request| {
            request.path().ends_with(service) && request.header("Authorization") == Some(&header)
        });
        assert!(sent, "no {service} request with the token");
    }

    // 5. The file names the variable, and holds neither the token nor a user part.
    let text = fs::read_to_string(remotes_file(&store)).expect("remotes.toml");
    assert!(text.contains(VAR), "{text}");
    assert!(!text.contains(TOKEN) && !text.contains('@'), "{text}");
}
