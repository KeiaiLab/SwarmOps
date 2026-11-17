//! A login's token on the way to a git remote, through the binary: `clone`, `fetch`, `push`,
//! `sync` and `remote add` send the token of the login kept for the remote's host to a remote
//! that names no `--token-env`, to that host alone, and never show it; and
//! `auth git-credential` hands it to a git that asks as git asks a credential helper, on stdout
//! and nowhere else. And the login itself by GitHub's device flow, against a stand-in for the
//! curl it runs ([`FakeCurl`]): no test reaches GitHub.
//!
//! The remote is the test kit's [`GitServer`] on `127.0.0.1`, behind HTTP basic authentication
//! that takes one token. `auth login --gh` keeps a login for `github.com` only, so each test
//! writes the login file itself, for the server's `127.0.0.1:<port>`, and names it with
//! `SWARMOPS_AUTH_FILE`; the binary reads it as it reads one that `auth login` wrote. The tokens
//! are placeholders in no provider's shape.

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde_json::Value;
use swarmops_testkit::curl::{CURL_ENV, FakeCurl};
use swarmops_testkit::remote::{GitServer, HttpAuth, HttpServer};
use tempfile::TempDir;

use crate::support::{Run, SO, Store, run, run_bin};

/// The agent the commands that need an identity run as.
const AGENT: &str = "agent/worker-17";

/// The repository every test serves.
const REPO: &str = "repo";

/// The remote's name in the store.
const ORIGIN: &str = "origin";

/// The user name a token goes with.
const USER: &str = "x-access-token";

/// The token the server takes.
const TOKEN: &str = "example-token-not-a-secret";

/// A token the server refuses.
const STALE: &str = "stale-token-not-a-secret";

/// The token of a login for another host, which must reach no server.
const ELSEWHERE: &str = "elsewhere-token-not-a-secret";

/// A token variable a remote names.
const VAR: &str = "SO_TEST_AUTH_TOKEN";

/// No `Authorization` header: what a server that was sent no token received.
const NO_HEADERS: [&str; 0] = [];

/// The variable that names the login file.
const AUTH_FILE: &str = "SWARMOPS_AUTH_FILE";

/// A served repository with two commits on `main` behind basic authentication, and what the
/// binary runs with: the server's git, an empty `HOME`, no system or global configuration, and
/// a login file of the test's own.
struct Site {
    /// The remote's side.
    server: GitServer,
    /// The server's smart HTTP, which takes [`TOKEN`].
    http: HttpServer,
    /// git's empty `HOME` and the login file's directory, removed when the site is dropped.
    home: TempDir,
    /// The variables every run sets.
    env: Vec<(String, String)>,
}

impl Site {
    fn new() -> Site {
        let server = GitServer::new();
        server.create(REPO);
        server.commit_on(REPO, "main", "first", &[("a.txt", "a\n")]);
        server.commit_on(REPO, "main", "second", &[("b.txt", "b\n")]);
        let http = server.http(HttpAuth::Basic {
            user: USER.to_owned(),
            token: TOKEN.to_owned(),
        });
        let home = tempfile::tempdir().expect("a temp HOME");
        let file = home.path().join("logins").join("auth.toml");
        let env = vec![
            ("SWARMOPS_GIT".to_owned(), path_text(&real_git(&server))),
            ("HOME".to_owned(), path_text(home.path())),
            ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
            ("GIT_CONFIG_GLOBAL".to_owned(), "/dev/null".to_owned()),
            (AUTH_FILE.to_owned(), path_text(&file)),
        ];
        Site {
            server,
            http,
            home,
            env,
        }
    }

    /// The login file's path: nothing is there until [`Site::log_in`] writes it.
    fn file(&self) -> PathBuf {
        self.home.path().join("logins").join("auth.toml")
    }

    /// Writes the login file, mode 0600, with one `gh` login per `(host, token)` of `logins`.
    fn log_in(&self, logins: &[(&str, &str)]) {
        let mut text = "version = 1\n".to_owned();
        for (host, token) in logins {
            write!(
                text,
                "\n[[login]]\nprovider = \"gh\"\nhost = \"{host}\"\ntoken = \"{token}\"\n\
                 source = \"with_token\"\n"
            )
            .expect("write to a string");
        }
        let file = self.file();
        fs::create_dir_all(file.parent().expect("a directory")).expect("make the directory");
        fs::write(&file, text).expect("write the login file");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).expect("set the mode");
    }

    /// The server's host as a login names it: `127.0.0.1:<port>`.
    fn host(&self) -> String {
        self.http.host()
    }

    /// The same address with another port.
    fn other_port(&self) -> String {
        let host = self.host();
        let (address, port) = host.rsplit_once(':').expect("a port");
        let port: u16 = port.parse().expect("a port number");
        format!(
            "{address}:{}",
            if port == u16::MAX { port - 1 } else { port + 1 }
        )
    }

    fn url(&self) -> String {
        self.http.url(REPO)
    }

    /// The variables every run sets.
    fn vars(&self) -> Vec<(&str, &str)> {
        let own = self.env.iter().map(|(k, v)| (k.as_str(), v.as_str()));
        own.collect()
    }

    /// Runs the binary with `args` and the site's variables.
    fn run(&self, args: &[&str]) -> Run {
        run(args, &self.vars())
    }

    /// A fresh store whose commands run with the site's variables, [`ORIGIN`] recorded with
    /// `--branch main` (no network call) and then `more`.
    fn store(&self, more: &[&str]) -> Store {
        let store = Store::init().with_env(&self.vars());
        let url = self.url();
        let mut line = vec!["remote", "add", ORIGIN, url.as_str(), "--branch", "main"];
        line.extend_from_slice(more);
        store.read(&line).ok();
        store
    }

    /// The `Authorization` header a request with `token` carries.
    fn header(token: &str) -> String {
        format!("Basic {}", base64(&format!("{USER}:{token}")))
    }
}

/// `text` in standard padded base64, as HTTP's Basic scheme carries a credential.
fn base64(text: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(text)
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

/// Every file under `root`.
fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).expect("a readable dir") {
            let path = entry.expect("an entry").path();
            if path.is_dir() {
                dirs.push(path);
                continue;
            }
            files.push(path);
        }
    }
    files
}

/// Commits `content` at `path` on `main` of `store` and moves `main` to it.
fn land(store: &Store, path: &str, content: &str) {
    let refs = store.read(&["ref", "list"]).ok();
    let listed = refs["refs"].as_array().expect("refs");
    let main = listed.iter().find(|entry| entry["name"] == "main");
    let old = main.expect("main")["target"].as_str().expect("a target");
    let new = store.commit(AGENT, path, content, Some(old));
    let line = ["ref", "set", "main", new.as_str(), "--expect", old];
    store.run_as(AGENT, &line, &[]).ok();
}

/// Asserts that `refused` is the remote's refusal for the credential, and gives its error.
fn auth_refusal(refused: &Run) -> Value {
    let error = refused.err("E_REMOTE_REJECTED");
    assert_eq!(error["exit"], 5, "{refused}");
    assert_eq!(error["details"]["reason"], "auth", "{refused}");
    let hint = error["hint"].as_str().expect("a hint");
    assert!(hint.contains("swarmops auth status"), "{hint}");
    error
}

/// Asserts that neither stream of `done` holds `token`, as it is or in the header's base64.
fn assert_unseen(done: &Run, token: &str) {
    let encoded = base64(&format!("{USER}:{token}"));
    for text in [&done.stdout, &done.stderr] {
        assert!(!text.contains(token) && !text.contains(&encoded), "{done}");
    }
}

/// The login for the remote's host is what every command that reaches the remote sends, and
/// what none of them shows or writes: with no login the clone is refused for the credential;
/// with one, `clone`, `remote add` (asking for the default branch), `fetch`, `push` and `sync`
/// succeed, every request carrying that token; the login gone, the fetch is refused again.
#[test]
fn a_login_is_used_by_clone_fetch_push_and_sync() {
    let site = Site::new();
    let url = site.url();
    let dir = tempfile::tempdir().expect("a temp dir");
    let at = |name: &str| path_text(&dir.path().join(name));

    // 1. No login: the server answers 401 and git has nothing to answer with. The hint names
    //    the login, and the error does not say one was used.
    let (early, into) = (at("early"), at("clone"));
    let refused = site.run(&["--as", AGENT, "clone", &url, "--into", &early]);
    let error = auth_refusal(&refused);
    assert!(error["details"].get("login_used").is_none(), "{refused}");
    assert_eq!(site.http.authorizations(), NO_HEADERS);

    // 2. A login for the server's host, beside one for another host: every remote command
    //    goes through. The clone makes one store; the fetch, the push and the sync run on
    //    another, whose main is made from the fetched head.
    site.log_in(&[("github.com", ELSEWHERE), (&site.host(), TOKEN)]);
    let before = site.server.rev_parse(REPO, "main").expect("main");
    let mut runs = vec![site.run(&["--as", AGENT, "clone", &url, "--into", &into])];
    let line = [
        "--repo",
        into.as_str(),
        "remote",
        "add",
        "second",
        url.as_str(),
    ];
    runs.push(site.run(&line));
    assert_eq!(runs[1].ok()["branch"], "main", "{}", runs[1]);
    let store = site.store(&[]);
    runs.push(store.run_as(AGENT, &["fetch", ORIGIN], &[]));
    let fetched = runs[2].ok();
    let head = fetched["head"]["changeset"].as_str().expect("a head");
    let line = ["ref", "set", "main", head, "--create"];
    store.run_as(AGENT, &line, &[]).ok();
    land(&store, "local.txt", "local\n");
    runs.push(store.run_as(AGENT, &["push", ORIGIN], &[]));
    let pushed = site.server.rev_parse(REPO, "main").expect("main");
    assert_ne!(pushed, before, "the push moved the remote's main");
    land(&store, "more.txt", "more\n");
    runs.push(store.run_as(AGENT, &["sync", ORIGIN], &[]));
    for done in &runs {
        assert_eq!(done.exit(), 0, "{done}");
    }
    let synced = site.server.rev_parse(REPO, "main").expect("main");
    assert_ne!(synced, pushed, "the sync moved the remote's main");

    // 3. The fetches' requests and the pushes' carried the login's token, and no request
    //    carried anything else: not the other host's token.
    let header = Site::header(TOKEN);
    let requests = site.http.requests();
    for service in ["git-upload-pack", "git-receive-pack"] {
        let sent = requests.iter().any(|request| {
            request.path().ends_with(service) && request.header("Authorization") == Some(&header)
        });
        assert!(sent, "no {service} request with the login's token");
    }
    let sent = site.http.authorizations();
    assert!(sent.iter().all(|seen| *seen == header), "{sent:?}");

    // 4. The token is in no output and in no file of the store: the login file alone has it.
    for done in &runs {
        assert_unseen(done, TOKEN);
        assert_unseen(done, ELSEWHERE);
    }
    let stores = [Path::new(&into), Path::new(store.path())];
    for file in stores.into_iter().flat_map(files_under) {
        let bytes = fs::read(&file).expect("a readable file");
        for secret in [TOKEN.to_owned(), base64(&format!("{USER}:{TOKEN}"))] {
            let found = bytes.windows(secret.len()).any(|w| w == secret.as_bytes());
            assert!(!found, "{} holds the token", file.display());
        }
    }
    let remotes = fs::read_to_string(Path::new(&into).join("remotes.toml")).expect("remotes");
    assert!(!remotes.contains("token"), "{remotes}");

    // 5. The login removed: the remote refuses again, and the hint says where to look.
    site.log_in(&[("github.com", ELSEWHERE)]);
    let refused = store.run_as(AGENT, &["fetch", ORIGIN], &[]);
    let error = auth_refusal(&refused);
    assert!(error["details"].get("login_used").is_none(), "{refused}");
}

/// A login is for one host and port: logins for `github.com`, for the server's address at
/// another port, for the address without a port and for a name of it reach no request.
#[test]
fn a_login_for_another_port_is_not_sent() {
    let site = Site::new();
    let host = site.host();
    let (address, port) = host.rsplit_once(':').expect("a port");
    let name = format!("localhost:{port}");
    site.log_in(&[
        ("github.com", ELSEWHERE),
        (&site.other_port(), TOKEN),
        (address, TOKEN),
        (&name, TOKEN),
    ]);
    let store = site.store(&[]);

    let refused = store.run_as(AGENT, &["fetch", ORIGIN], &[]);
    let error = auth_refusal(&refused);
    assert!(error["details"].get("login_used").is_none(), "{refused}");
    assert!(!site.http.requests().is_empty(), "the remote was asked");
    assert_eq!(site.http.authorizations(), NO_HEADERS);
    assert_unseen(&refused, TOKEN);
}

/// A login whose token the remote refuses is the remote's refusal for the credential, and the
/// error says that the login's token was what it refused (`details.login_used`), without the
/// token.
#[test]
fn a_stale_login_is_an_auth_rejection() {
    let site = Site::new();
    site.log_in(&[(&site.host(), STALE)]);
    let store = site.store(&[]);

    let fetched = store.run_as(AGENT, &["fetch", ORIGIN], &[]);
    let synced = store.run_as(AGENT, &["sync", ORIGIN], &[]);
    for refused in [&fetched, &synced] {
        let error = auth_refusal(refused);
        assert_eq!(error["details"]["login_used"], true, "{refused}");
        assert!(
            error["details"].get("token_env_unset").is_none(),
            "{refused}"
        );
        assert_unseen(refused, STALE);
    }
    assert!(site.http.authorizations().contains(&Site::header(STALE)));
}

/// A login file that is there and is refused fails the command before git runs, whatever
/// the file holds: the remote is not asked without the token. A remote that names its variable
/// never opens the file, and goes on as before.
#[test]
fn a_loose_login_file_fails_the_fetch_closed() {
    let site = Site::new();
    site.log_in(&[(&site.host(), TOKEN)]);
    let file = site.file();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).expect("set the mode");
    let store = site.store(&[]);
    let dir = tempfile::tempdir().expect("a temp dir");
    let into = path_text(&dir.path().join("clone"));
    let url = site.url();

    // 1. Each command that would reach the remote is refused for the file, and names it.
    let refusals = [
        store.run_as(AGENT, &["fetch", ORIGIN], &[]),
        store.run_as(AGENT, &["sync", ORIGIN], &[]),
        store.read(&["remote", "add", "second", &url]),
        site.run(&["--as", AGENT, "clone", &url, "--into", &into]),
    ];
    for refused in &refusals {
        let error = refused.err("E_INVALID_ARG");
        assert_eq!(error["details"]["reason"], "auth_file_mode", "{refused}");
        assert_eq!(error["details"]["auth_file"], path_text(&file), "{refused}");
        assert_unseen(refused, TOKEN);
    }
    assert!(site.http.requests().is_empty(), "nothing was sent");

    // 2. A remote that names its variable does not open the file.
    let named = site.store(&["--token-env", VAR]);
    named
        .run_as(AGENT, &["fetch", ORIGIN], &[(VAR, TOKEN)])
        .ok();

    // 3. The mode fixed as the hint says, the first store fetches with the login.
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).expect("set the mode");
    store.run_as(AGENT, &["fetch", ORIGIN], &[]).ok();
}

/// A remote that names a token variable goes with that variable alone: with the variable
/// unset, the login for the same host is not sent in its place, and the error says the
/// variable was unset.
#[test]
fn a_named_variable_excludes_the_login() {
    let site = Site::new();
    site.log_in(&[(&site.host(), TOKEN)]);
    let store = site.store(&["--token-env", VAR]);

    let refused = store.run_as(AGENT, &["fetch", ORIGIN], &[]);
    let error = auth_refusal(&refused);
    assert_eq!(error["details"]["token_env_unset"], true, "{refused}");
    assert!(error["details"].get("login_used").is_none(), "{refused}");
    assert_eq!(site.http.authorizations(), NO_HEADERS);

    // A wrong token in the variable is refused as the variable's, not the login's.
    let refused = store.run_as(AGENT, &["fetch", ORIGIN], &[(VAR, STALE)]);
    let error = auth_refusal(&refused);
    assert!(error["details"].get("login_used").is_none(), "{refused}");
    assert_eq!(site.http.authorizations(), [Site::header(STALE)]);
}

/// The helper's command line: the binary by its short name, with git's operation as its last
/// word.
const HELPER: [&str; 2] = ["auth", "git-credential"];

/// Runs `auth git-credential <operation>` with the site's variables and `request` on stdin.
fn helper(site: &Site, operation: &str, request: &str) -> Run {
    let line = [HELPER[0], HELPER[1], operation];
    run_bin(SO, &line, &site.vars(), request)
}

/// The envelope a run of the helper that failed left on stderr, its stdout empty: checked for
/// the exit code and given as its error.
fn helper_error(failed: &Run, code: &str) -> Value {
    assert_eq!(failed.stdout, "", "{failed}");
    let envelope: Value = serde_json::from_str(&failed.stderr)
        .unwrap_or_else(|e| panic!("stderr is one JSON document ({e}): {failed}"));
    assert_eq!(envelope["ok"], false, "{failed}");
    assert_eq!(envelope["op"], "auth_git_credential", "{failed}");
    assert_eq!(envelope["error"]["code"], code, "{failed}");
    assert_eq!(envelope["error"]["exit"], failed.exit(), "{failed}");
    envelope["error"].clone()
}

/// `auth git-credential get` answers git's request for a login's host with exactly a
/// `username=` and a `password=` line on stdout and nothing on stderr; a request no login is
/// for, and `store`, `erase` and any other word, print nothing at all and exit 0; a refused
/// login file and a missing operation leave stdout empty and their envelope on stderr.
#[test]
fn git_credential_get_answers_git() {
    let site = Site::new();
    let host = site.host();
    let quiet = |done: &Run| {
        assert_eq!(done.exit(), 0, "{done}");
        assert_eq!(
            (done.stdout.as_str(), done.stderr.as_str()),
            ("", ""),
            "{done}"
        );
    };
    let loopback = format!("protocol=http\nhost={host}\n\n");
    let github = "protocol=https\nhost=github.com\n\n";

    // 1. No login file: nothing to answer with, and nothing said.
    quiet(&helper(&site, "get", github));

    // 2. A login for the server's host and one for github.com: each request gets its own
    //    host's token, as git writes the request (keys this build does not know among them).
    site.log_in(&[("github.com", ELSEWHERE), (&host, TOKEN)]);
    let asked = format!(
        "capability[]=authtype\ncapability[]=state\nprotocol=http\nhost={host}\n\
         path={REPO}.git\nwwwauth[]=Basic realm=\"git\"\n\n"
    );
    for (request, token) in [
        (loopback.as_str(), TOKEN),
        (asked.as_str(), TOKEN),
        (github, ELSEWHERE),
        ("protocol=https\nhost=GitHub.com:443\n", ELSEWHERE),
    ] {
        let answered = helper(&site, "get", request);
        assert_eq!(answered.exit(), 0, "{answered}");
        let lines = format!("username={USER}\npassword={token}\n");
        assert_eq!(answered.stdout, lines, "{answered}");
        assert_eq!(answered.stderr, "", "{answered}");
    }

    // 3. No answer: another host, a subdomain, another port, github.com over ssh
    //    and over http, and a request that names no host.
    let other_port = format!("protocol=http\nhost={}\n\n", site.other_port());
    for request in [
        "protocol=https\nhost=example.com\n\n",
        "protocol=https\nhost=gist.github.com\n\n",
        "protocol=https\nhost=github.com:8443\n\n",
        "protocol=ssh\nhost=github.com\n\n",
        "protocol=http\nhost=github.com\n\n",
        other_port.as_str(),
        "protocol=https\n\n",
        "",
    ] {
        quiet(&helper(&site, "get", request));
    }

    // 4. store, erase and any other word: nothing printed, and the file as it was, though the
    //    request names the login's host and another password.
    let before = fs::read(site.file()).expect("the login file");
    let told = format!("protocol=http\nhost={host}\nusername={USER}\npassword={STALE}\n\n");
    for operation in ["store", "erase", "frobnicate", "GET"] {
        quiet(&helper(&site, operation, &told));
    }
    assert_eq!(fs::read(site.file()).expect("the login file"), before);

    // 5. A login file that is refused: nothing on stdout, the envelope on stderr with its exit
    //    code, and no token in it. store and erase do not open the file.
    fs::set_permissions(site.file(), fs::Permissions::from_mode(0o644)).expect("set the mode");
    let refused = helper(&site, "get", &loopback);
    let error = helper_error(&refused, "E_INVALID_ARG");
    assert_eq!(refused.exit(), 2, "{refused}");
    assert_eq!(error["details"]["reason"], "auth_file_mode", "{refused}");
    let file = path_text(&site.file());
    assert_eq!(error["details"]["auth_file"], file, "{refused}");
    assert_unseen(&refused, TOKEN);
    quiet(&helper(&site, "store", &told));
    quiet(&helper(&site, "erase", &told));

    // 6. No operation, or two: a usage error, on stderr too.
    for line in [&HELPER[..], &["auth", "git-credential", "get", "more"]] {
        let refused = run_bin(SO, line, &site.vars(), &loopback);
        helper_error(&refused, "E_INVALID_ARG");
        assert_eq!(refused.exit(), 2, "{refused}");
    }
}

/// A plain git that is told to use the helper gets through the remote's basic authentication
/// with the login's token, and shows it nowhere; without the helper, or with a login the
/// request is not for, the same git is refused. A store's own fetch works the same with the
/// helper in git's global configuration.
#[test]
fn plain_git_uses_the_helper() {
    let site = Site::new();
    let url = site.url();
    assert!(!SO.contains('\''), "{SO}");
    let setting = format!("credential.helper=!'{SO}' {} {}", HELPER[0], HELPER[1]);
    // git in the server's pinned environment, which has no variable of this process's: the
    // helper it starts inherits that, and the login file named here.
    let ls_remote = |with_helper: bool| {
        let mut git = site.server.command();
        git.env(AUTH_FILE, site.file());
        if with_helper {
            git.args(["-c", setting.as_str()]);
        }
        git.args(["ls-remote", url.as_str()]);
        git.output().expect("run git")
    };
    let said = |output: &std::process::Output| {
        let out = String::from_utf8_lossy(&output.stdout);
        format!("{out}{}", String::from_utf8_lossy(&output.stderr))
    };
    let main = site.server.rev_parse(REPO, "main").expect("main");

    // 1. No helper: the server asks for a password and git has none to give.
    site.log_in(&[("github.com", ELSEWHERE), (&site.host(), TOKEN)]);
    let refused = ls_remote(false);
    assert!(!refused.status.success(), "{}", said(&refused));
    assert_eq!(site.http.authorizations(), NO_HEADERS);

    // 2. With the helper git asks it after the 401 and comes back with the login's token, and
    //    with no other login's.
    let listed = ls_remote(true);
    assert!(listed.status.success(), "{}", said(&listed));
    assert!(said(&listed).contains(&main), "{}", said(&listed));
    assert!(!said(&listed).contains(TOKEN), "{}", said(&listed));
    let sent = site.http.authorizations();
    assert!(!sent.is_empty(), "no request carried a credential");
    assert!(
        sent.iter().all(|seen| *seen == Site::header(TOKEN)),
        "{sent:?}"
    );

    // 3. A login for another port only: the helper has no answer, and git is refused as
    //    without it.
    site.log_in(&[("github.com", ELSEWHERE), (&site.other_port(), TOKEN)]);
    let refused = ls_remote(true);
    assert!(!refused.status.success(), "{}", said(&refused));
    assert_eq!(
        site.http.authorizations().len(),
        sent.len(),
        "nothing more was sent"
    );

    // 4. The helper in git's global configuration: a store's fetch, which sends the login's
    //    token itself, works with it there.
    site.log_in(&[(&site.host(), TOKEN)]);
    let config = site.home.path().join("gitconfig");
    let helper = format!("!'{SO}' {} {}", HELPER[0], HELPER[1]);
    fs::write(&config, format!("[credential]\n\thelper = \"{helper}\"\n")).expect("write");
    let store = site.store(&[]);
    let global = [("GIT_CONFIG_GLOBAL", config.to_str().expect("UTF-8"))];
    let fetched = store.run_as(AGENT, &["fetch", ORIGIN], &global);
    assert_eq!(fetched.exit(), 0, "{fetched}");
    assert_unseen(&fetched, TOKEN);
    let sent = site.http.authorizations();
    assert!(
        sent.iter().all(|seen| *seen == Site::header(TOKEN)),
        "{sent:?}"
    );
}

/// The client ID a login by the device flow is asked in the name of: no provider issued it.
const CLIENT: &str = "example-client-id";

/// The device code of the stand-in's answers, which nothing may show.
const DEVICE: &str = "example-device-code-not-a-secret";

/// The code a person would enter.
const USER_CODE: &str = "ABCD-1234";

/// What a login by the device flow runs with: a stand-in for curl, a client ID, and a login
/// file of the test's own in an empty `HOME`.
struct Flow {
    /// The stand-in, with the answers the test queued.
    curl: FakeCurl,
    /// The login file's directory, removed when the flow is dropped.
    home: TempDir,
}

impl Flow {
    fn new() -> Flow {
        Flow {
            curl: FakeCurl::new(),
            home: tempfile::tempdir().expect("a temp HOME"),
        }
    }

    /// The login file's path: nothing is there until a login writes it.
    fn file(&self) -> PathBuf {
        self.home.path().join("logins").join("auth.toml")
    }

    /// Runs `auth <line>` with the flow's variables, then `more`, and nothing on stdin.
    fn auth(&self, line: &[&str], more: &[(&str, &str)]) -> Run {
        let (file, curl) = (path_text(&self.file()), path_text(&self.curl.path()));
        let mut env = vec![
            ("HOME", path_text(self.home.path())),
            (AUTH_FILE, file),
            (CURL_ENV, curl),
            ("SWARMOPS_GH_CLIENT_ID", CLIENT.to_owned()),
        ];
        env.extend(
            more.iter()
                .map(|(name, value)| (*name, (*value).to_owned())),
        );
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let mut args = vec!["auth"];
        args.extend_from_slice(line);
        run_bin(SO, &args, &env, "")
    }

    /// Runs `auth login --gh`.
    fn login(&self) -> Run {
        self.auth(&["login", "--gh"], &[])
    }
}

/// The first answer of a flow that goes on: a code polled every second, the shortest wait
/// there is, so a test with one poll takes about a second.
fn device_code() -> String {
    serde_json::json!({
        "device_code": DEVICE, "user_code": USER_CODE,
        "verification_uri": "https://github.com/login/device",
        "expires_in": 900, "interval": 1,
    })
    .to_string()
}

/// A poll's answer that names `error`.
fn poll_error(error: &str) -> String {
    serde_json::json!({"error": error, "error_description": "example words"}).to_string()
}

/// Asserts that neither stream of `done` holds the device code, the token or GitHub's words.
fn assert_no_secret(done: &Run) {
    for text in [&done.stdout, &done.stderr] {
        for secret in [DEVICE, TOKEN, "example words"] {
            assert!(!text.contains(secret), "{secret}: {done}");
        }
    }
}

/// `auth login --gh` without `--with-token` runs the device flow through curl and keeps the
/// token it ends with: the code and its page on stderr, one envelope on stdout, the request on
/// curl's stdin and never in its arguments, curl's environment the allowed variables alone,
/// and the device code and the token in neither stream. It takes about two seconds: a
/// one-second wait before each of two polls.
#[test]
fn device_flow_logs_in() {
    let flow = Flow::new();
    let granted = serde_json::json!({"access_token": TOKEN, "token_type": "bearer",
                                     "scope": "repo,workflow"});
    flow.curl
        .answer(200, &device_code())
        .answer(200, &poll_error("authorization_pending"))
        .answer(200, &granted.to_string());

    // 1. The login: its result on stdout, what a person needs on stderr, no secret in either.
    let planted = [
        ("SO_TEST_PLANTED", "planted"),
        ("HTTPS_PROXY", "http://127.0.0.1:9"),
    ];
    let line = ["login", "--gh", "--scope", "repo", "--scope", "workflow"];
    let done = flow.auth(&line, &planted);
    let file = flow.file();
    let entry = serde_json::json!({"provider": "gh", "host": "github.com", "source": "device",
                                   "scopes": ["repo", "workflow"]});
    assert_eq!(
        done.ok(),
        serde_json::json!({"login": entry, "file": path_text(&file), "replaced": false})
    );
    assert_eq!(
        done.stderr,
        format!(
            "Open https://github.com/login/device and enter the code {USER_CODE}\n\
             Waiting for approval (the code expires in 15 minutes).\n"
        )
    );
    assert_no_secret(&done);

    // 2. The file: the user's alone, holding the token and where it came from.
    let mode = fs::metadata(&file).expect("stat").permissions().mode() & 0o7777;
    assert_eq!(mode, 0o600);
    let kept = fs::read_to_string(&file).expect("read the login file");
    assert!(kept.contains(&format!("token = \"{TOKEN}\"")), "{kept}");
    assert!(kept.contains("source = \"device\""), "{kept}");
    let listed = flow.auth(&["status"], &[]).ok();
    assert_eq!(listed["logins"], serde_json::json!([entry]));

    // 3. Three runs of curl: one for the code, two polls. The whole request is on stdin, to
    //    GitHub's two addresses and no other, with no redirect followed.
    let calls = flow.curl.calls();
    assert_eq!(calls.len(), 3);
    for call in &calls {
        assert_eq!(call.args, ["-q", "--config", "-"]);
        assert!(call.stdin.contains("proto = \"=https\"\n"), "{call:?}");
        assert!(call.stdin.contains("max-redirs = 0\n"), "{call:?}");
        assert!(!call.stdin.contains("location"), "{call:?}");
    }
    let ask = &calls[0].stdin;
    assert!(
        ask.starts_with("url = \"https://github.com/login/device/code\"\n"),
        "{ask}"
    );
    let form = format!("data = \"client_id={CLIENT}&scope=repo%20workflow\"\n");
    assert!(ask.contains(&form), "{ask}");
    assert_eq!(calls[1].stdin, calls[2].stdin, "the same poll each time");
    let poll = &calls[1].stdin;
    assert!(
        poll.starts_with("url = \"https://github.com/login/oauth/access_token\"\n"),
        "{poll}"
    );
    let form = format!("data = \"client_id={CLIENT}&device_code={DEVICE}&grant_type=");
    assert!(poll.contains(&form), "{poll}");

    // 4. curl's environment: where programs are and the proxy, and nothing else of the
    //    caller's, the login file's variable and the home directory included.
    let env = &calls[0].env;
    let has = |name: &str| env.iter().any(|kept| kept == name);
    assert!(has("PATH") && has("HTTPS_PROXY"), "{env:?}");
    for dropped in ["SO_TEST_PLANTED", "HOME", AUTH_FILE, CURL_ENV] {
        assert!(!has(dropped), "{dropped}: {env:?}");
    }
    assert!(
        !env.iter().any(|kept| kept.starts_with("SWARMOPS_")),
        "{env:?}"
    );
}

/// Each way the device flow ends without a token: the error's code, exit and
/// `details.reason`, one envelope on stdout, no login file, and no device code, token or word
/// of GitHub's in either stream. A refusal of the first answer shows nothing on stderr: the
/// code is shown only once it has been checked. The four cases that poll take a second each.
#[test]
fn device_flow_refusals() {
    let refresh = serde_json::json!({"access_token": TOKEN, "expires_in": 28800,
                                     "refresh_token": "example-refresh-not-a-secret"});
    let elsewhere = device_code().replace("github.com", "example.com");
    let escape = device_code().replace(USER_CODE, "AB\\u001b[2JCD");
    let polled = |answer: &str| vec![(200, device_code()), (200, answer.to_owned())];
    let cases = [
        ("access_denied", polled(&poll_error("access_denied"))),
        ("expired_token", polled(&poll_error("expired_token"))),
        ("expiring_token", polled(&refresh.to_string())),
        ("unexpected_answer", polled("<html>Bad Gateway</html>")),
        (
            "device_flow_disabled",
            vec![(200, poll_error("device_flow_disabled"))],
        ),
        ("bad_client_id", vec![(404, poll_error("Not Found"))]),
        ("unexpected_answer", vec![(200, "<html>".to_owned())]),
        ("unexpected_answer", vec![(200, elsewhere)]),
        ("unexpected_answer", vec![(200, escape)]),
    ];
    for (reason, answers) in cases {
        let flow = Flow::new();
        for (status, body) in &answers {
            flow.curl.answer(*status, body);
        }
        let refused = flow.login();
        let error = refused.err("E_AUTH");
        assert_eq!(error["exit"], 5, "{refused}");
        assert_eq!(error["details"]["reason"], reason, "{refused}");
        assert_eq!(error["details"]["provider"], "gh", "{refused}");
        assert_eq!(error["details"]["host"], "github.com", "{refused}");
        assert_no_secret(&refused);
        assert!(!refused.stderr.contains('\u{1b}'), "{refused}");
        assert!(!refused.stderr.contains("example.com"), "{refused}");
        assert_eq!(answers.len() == 1, refused.stderr.is_empty(), "{refused}");
        assert_eq!(flow.curl.calls().len(), answers.len(), "{refused}");
        assert!(!flow.file().exists(), "{reason}: no login is kept");
    }

    // curl reached no answer: the network's failure, to run again, with curl's exit code.
    let flow = Flow::new();
    flow.curl.fail(6);
    let refused = flow.login();
    let error = refused.err("E_REMOTE_UNREACHABLE");
    assert_eq!(
        (&error["exit"], &error["retryable"]),
        (&4.into(), &true.into())
    );
    let want = serde_json::json!({"phase": "login", "host": "github.com", "cause": "dns",
                                  "curl_exit": 6});
    assert_eq!(error["details"], want, "{refused}");
    assert_eq!(refused.stderr, "", "{refused}");
    assert!(!flow.file().exists(), "no login is kept");

    // No curl at all: said before anything is asked.
    let flow = Flow::new();
    let gone = path_text(&flow.home.path().join("no-curl"));
    let refused = flow.auth(&["login", "--gh"], &[(CURL_ENV, &gone)]);
    let error = refused.err("E_INVALID_ARG");
    assert_eq!(error["details"]["reason"], "curl_missing", "{refused}");
    assert_eq!(flow.curl.calls().len(), 0, "{refused}");
}

/// A login that could not be kept asks GitHub for nothing: a login file that is refused, and a
/// login at that host that is another provider's, fail before curl runs, so nobody approves a
/// code in vain.
#[test]
fn a_login_that_could_not_be_kept_asks_for_no_code() {
    for (mode, text, reason) in [
        (0o644, "version = 1\n".to_owned(), "auth_file_mode"),
        (
            0o600,
            "version = 1\n[[login]]\nprovider = \"so\"\nhost = \"github.com\"\n".to_owned(),
            "provider_mismatch",
        ),
    ] {
        let flow = Flow::new();
        flow.curl.answer(200, &device_code());
        let file = flow.file();
        fs::create_dir_all(file.parent().expect("a directory")).expect("make the directory");
        fs::write(&file, &text).expect("write the login file");
        fs::set_permissions(&file, fs::Permissions::from_mode(mode)).expect("set the mode");

        let refused = flow.login();
        let error = refused.err("E_INVALID_ARG");
        assert_eq!(error["details"]["reason"], reason, "{refused}");
        assert_eq!(refused.stderr, "", "{refused}");
        assert_eq!(flow.curl.calls().len(), 0, "{refused}");
        assert_eq!(fs::read_to_string(&file).expect("read"), text, "left alone");
    }
}
