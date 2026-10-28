//! `swarmops serve`'s test site (M14): a temporary `--root` of stores, served through an ssh
//! forced-command stand-in over pipes and as a CGI script behind the loopback HTTP server, so
//! that the serve tests never need the network or an sshd.
//!
//! ```text
//!   client git ── ssh://git@remote.invalid/<name>.git
//!       GIT_SSH_COMMAND = the stand-in: ssh's options and the host dropped,
//!       SSH_ORIGINAL_COMMAND = the rest joined by one space, GIT_PROTOCOL kept
//!         └── exec swarmops serve --root <root> --as <identity>
//!   client git ── http://127.0.0.1:<port>/<name>.git ── HttpServer (remote.rs)
//!         └── per request: swarmops serve --cgi --root <root> (--as <id> | --user-family <f>)
//!             with the RFC 3875 metavariables, REMOTE_USER from Basic auth
//! ```
//!
//! Every `swarmops` the site starts gets a cleared environment, as sshd and a web server give
//! a forced command and a CGI script theirs: `PATH` (with [`GIT_ENV`]'s directory first), an
//! empty `HOME`, `TMPDIR` and [`GIT_ENV`] when set, `LC_ALL=C`, `TZ=UTC`, the test runs'
//! variables of [`crate::cli`] (the order check, the memo keys' directory), and what
//! [`ServeSite::with_env`] adds. No `SWARMOPS_AGENT` of the developer's reaches it.

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use tempfile::TempDir;

use crate::cli::{Program, run_bin, swarmops};
use crate::remote::{
    Backend, GIT_ENV, GitServer, HttpAuth, HttpServer, SSH_HOST, path_str, sh_quote, write_script,
};

/// Who `swarmops serve --cgi` names the caller as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpIdentity {
    /// `--as <id>`: every request is this identity's.
    Fixed(String),
    /// `--user-family <family>`: the request's `REMOTE_USER` in this family.
    UserFamily(String),
}

/// A temporary root of stores that `swarmops serve` serves: through [`ServeSite::ssh_command`]
/// and [`ServeSite::http`].
#[derive(Debug)]
pub struct ServeSite {
    /// `root/` (the `--root`), `home/` (the children's empty `HOME`) and `ssh-<n>/` (the
    /// stand-ins).
    dir: TempDir,
    /// The `swarmops` binary.
    program: PathBuf,
    /// The client side's git, in the pinned environment.
    git: GitServer,
    /// What [`ServeSite::with_env`] added to the children's environment.
    extra: Vec<(OsString, OsString)>,
    /// The stand-ins written so far, for the next one's directory.
    stand_ins: AtomicU64,
}

impl ServeSite {
    /// An empty root served by `P`'s binary. Fails the test when git is missing or older than
    /// [`crate::remote::MIN_GIT`], naming [`GIT_ENV`].
    pub fn new<P: Program>() -> ServeSite {
        ServeSite::with_program(PathBuf::from(P::BIN))
    }

    /// An empty root served by `program`.
    fn with_program(program: PathBuf) -> ServeSite {
        let dir = tempfile::tempdir().expect("create the site's directory");
        for sub in ["root", "home"] {
            fs::create_dir(dir.path().join(sub)).expect("create a site directory");
        }
        ServeSite {
            dir,
            program,
            git: GitServer::new(),
            extra: Vec::new(),
            stand_ins: AtomicU64::new(0),
        }
    }

    /// `env` added to the environment of every `swarmops` the site starts from now on.
    #[must_use]
    pub fn with_env(mut self, env: &[(&str, &str)]) -> ServeSite {
        let env = env
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)));
        self.extra.extend(env);
        self
    }

    /// The `--root` the site serves.
    pub fn root(&self) -> PathBuf {
        self.dir.path().join("root")
    }

    /// The path of store `name` under the root.
    pub fn store(&self, name: &str) -> PathBuf {
        self.root().join(name)
    }

    /// Makes store `name` under the root (`swarmops init`), and returns its path.
    pub fn init(&self, name: &str) -> PathBuf {
        let path = self.store(name);
        let program = path_str(&self.program);
        run_bin(program, &["init", path_str(&path)], &[], "").ok();
        path
    }

    /// A client git command in the pinned environment ([`GitServer::command`]).
    pub fn command(&self) -> Command {
        self.git.command()
    }

    /// The stand-in for `GIT_SSH_COMMAND` of a key whose `authorized_keys` line is
    /// `command="swarmops serve --root <root> --as <identity>"`. Its URLs are
    /// [`ServeSite::ssh_url`]'s.
    pub fn ssh_command(&self, identity: &str) -> String {
        let at = self.stand_ins.fetch_add(1, Ordering::Relaxed);
        let dir = self.dir.path().join(format!("ssh-{at}"));
        fs::create_dir(&dir).expect("create the stand-in's directory");

        // The file is named ssh, so that git takes it for OpenSSH and does not probe it.
        let program = dir.join("ssh");
        write_script(&program, &self.ssh_script(identity));
        sh_quote(path_str(&program))
    }

    /// The ssh URL of store `name`: `ssh://git@remote.invalid/<name>.git`.
    pub fn ssh_url(&self, name: &str) -> String {
        format!("ssh://git@{SSH_HOST}/{name}.git")
    }

    /// Serves the root as `swarmops serve --cgi` behind git's smart HTTP on `127.0.0.1`, one
    /// run a request, until the returned server is stopped or dropped. [`HttpServer::url`]
    /// names a store.
    pub fn http(&self, identity: HttpIdentity, auth: HttpAuth) -> HttpServer {
        let (flag, value) = match identity {
            HttpIdentity::Fixed(id) => ("--as", id),
            HttpIdentity::UserFamily(family) => ("--user-family", family),
        };
        let root = self.root();
        let args = ["serve", "--cgi", "--root", path_str(&root), flag, &value];
        let backend = Backend::Cgi {
            program: self.program.clone(),
            args: args.iter().map(OsString::from).collect(),
            env: self.child_env(),
        };
        HttpServer::start(backend, auth)
    }

    /// The environment of every `swarmops` the site starts (the module's documentation).
    fn child_env(&self) -> Vec<(OsString, OsString)> {
        // 1. What sshd and a web server give: a PATH, a HOME, the locale.
        let mut env: Vec<(OsString, OsString)> = vec![
            ("PATH".into(), self.git.path_env().clone()),
            ("HOME".into(), self.dir.path().join("home").into()),
            ("LC_ALL".into(), "C".into()),
            ("TZ".into(), "UTC".into()),
        ];
        for name in ["TMPDIR", GIT_ENV] {
            if let Some(value) = std::env::var_os(name) {
                env.push((name.into(), value));
            }
        }

        // 2. The variables every test run of the binary sets, then the test's own.
        env.extend(swarmops_run_env());
        env.extend(self.extra.iter().cloned());
        env
    }

    /// The stand-in's script: skip ssh's options as [`crate::remote::SshMode::Serve`] does,
    /// drop the host, and exec the forced command with the rest as `SSH_ORIGINAL_COMMAND`.
    fn ssh_script(&self, identity: &str) -> String {
        let mut env = String::new();
        for (name, value) in self.child_env() {
            let name = name.to_str().expect("an ASCII variable name");
            let value = value.to_str().expect("a UTF-8 variable value");
            env.push_str(&sh_quote(&format!("{name}={value}")));
            env.push(' ');
        }
        format!(
            "#!/bin/sh\n\
             # ssh's options: -o, -p, -l and -i take a value; -4 and -6 none.\n\
             while [ $# -gt 0 ]; do\n\
             \x20 case \"$1\" in\n\
             \x20   -o|-p|-l|-i) shift 2 ;;\n\
             \x20   -*) shift ;;\n\
             \x20   *) break ;;\n\
             \x20 esac\n\
             done\n\
             # The host goes; OpenSSH joins the remaining words with one space.\n\
             shift\n\
             original=\"$*\"\n\
             # sshd's environment, not the client's: GIT_PROTOCOL alone crosses (AcceptEnv).\n\
             exec env -i {env}SSH_ORIGINAL_COMMAND=\"$original\" \
             ${{GIT_PROTOCOL+\"GIT_PROTOCOL=$GIT_PROTOCOL\"}} \
             {program} serve --root {root} --as {identity}\n",
            program = sh_quote(path_str(&self.program)),
            root = sh_quote(path_str(&self.root())),
            identity = sh_quote(identity),
        )
    }
}

/// The variables [`crate::cli::swarmops`] sets on every test run of the binary, read off the
/// command it builds, so that the two never drift apart.
fn swarmops_run_env() -> Vec<(OsString, OsString)> {
    /// A [`Program`] for the one call below: only the variables are read, never the binary.
    struct Any;
    impl Program for Any {
        const BIN: &'static str = "swarmops";
        const NOW_ENV: &'static str = "";
    }
    let command = swarmops::<Any, &str>(&[], &[]);

    // The removed variables (the caller's `SWARMOPS_*`) have no value: a cleared environment
    // has none of them anyway.
    command
        .get_envs()
        .filter_map(|(name, value)| Some((name.to_owned(), value?.to_owned())))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::path::Path;

    use super::{HttpIdentity, ServeSite};
    use crate::child::{self, CHILD_TIMEOUT};
    use crate::remote::{HttpAuth, HttpServer, base64, write_script};

    /// The store every test names.
    const STORE: &str = "acme";

    /// A site whose `swarmops` is the script `body` (after `#!/bin/sh`), written in `dir`.
    fn scripted(dir: &Path, body: &str) -> ServeSite {
        let program = dir.join("swarmops");
        write_script(&program, &format!("#!/bin/sh\n{body}"));
        ServeSite::with_program(program)
    }

    /// The `NAME=value` lines of a recorded environment.
    fn variables(record: &str) -> BTreeMap<String, String> {
        record
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect()
    }

    /// Sends `raw` to `http` and reads the whole answer: its status and body.
    fn exchange(http: &HttpServer, raw: &[u8]) -> (u16, String) {
        let mut stream = TcpStream::connect(http.host()).expect("connect to the server");
        stream
            .set_read_timeout(Some(CHILD_TIMEOUT))
            .expect("set a read timeout");
        stream.write_all(raw).expect("send the request");
        let mut answer = String::new();
        stream
            .read_to_string(&mut answer)
            .expect("read the answer before the deadline");
        let status = answer
            .split(' ')
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or_else(|| panic!("no status line: {answer}"));
        let body = answer
            .split_once("\r\n\r\n")
            .map_or("", |(_, body)| body)
            .to_owned();
        (status, body)
    }

    /// A GET of `/acme.git/info/refs` with no header beyond `Host`.
    fn get(http: &HttpServer) -> (u16, String) {
        let raw = format!(
            "GET /{STORE}.git/info/refs HTTP/1.1\r\nHost: {}\r\n\r\n",
            http.host()
        );
        exchange(http, raw.as_bytes())
    }

    #[test]
    fn forced_command_gets_the_original_command() {
        let work = tempfile::tempdir().expect("a temp dir");
        let record = work.path().join("record");
        let site = scripted(
            work.path(),
            &format!(
                "{{ printf 'arg=%s\\n' \"$@\"; env; }} > '{}'\nexit 3\n",
                record.display()
            ),
        )
        .with_env(&[("SWARMOPS_TEST_EXTRA", "1")]);
        let ssh = site.ssh_command("person/ada");

        // 1. git runs the stand-in, which runs the forced command; that one refuses.
        let mut command = site.command();
        command
            .env("GIT_SSH_COMMAND", &ssh)
            .env("SWARMOPS_AGENT", "person/eve")
            .args(["-c", "protocol.version=2", "ls-remote"])
            .arg(site.ssh_url(STORE));
        let output = child::run(&mut command);
        assert!(!output.status.success(), "the refusing command succeeded");

        // 2. Its arguments: the authorized_keys line's command.
        let record = std::fs::read_to_string(&record).expect("the command ran");
        let root = site.root();
        let args: Vec<&str> = record
            .lines()
            .filter_map(|line| line.strip_prefix("arg="))
            .collect();
        let root = root.to_str().expect("UTF-8");
        assert_eq!(
            args,
            ["serve", "--root", root, "--as", "person/ada"],
            "{record}"
        );

        // 3. Its environment: OpenSSH's SSH_ORIGINAL_COMMAND verbatim, the protocol kept, the
        //    site's variables, and none of the client's.
        let env = variables(&record);
        let original = env.get("SSH_ORIGINAL_COMMAND").map(String::as_str);
        assert_eq!(original, Some("git-upload-pack '/acme.git'"), "{record}");
        let protocol = env.get("GIT_PROTOCOL").map(String::as_str);
        assert_eq!(protocol, Some("version=2"), "{record}");
        assert_eq!(
            env.get("SWARMOPS_TEST_EXTRA").map(String::as_str),
            Some("1")
        );
        assert!(!env.contains_key("SWARMOPS_AGENT"), "{record}");
        assert!(!env.contains_key("GIT_SSH_COMMAND"), "{record}");
    }

    #[test]
    fn forced_command_serves_over_pipes() {
        let work = tempfile::tempdir().expect("a temp dir");
        let bare = work.path().join("served.git");
        let site = scripted(
            work.path(),
            &format!("exec git upload-pack '{}'\n", bare.display()),
        );

        // A bare repository with one commit, which the forced command serves.
        let source = work.path().join("source");
        for args in [
            &[
                "init",
                "--quiet",
                "-b",
                "main",
                source.to_str().expect("UTF-8"),
            ][..],
            &[
                "-C",
                source.to_str().expect("UTF-8"),
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "one",
            ],
            &[
                "clone",
                "--quiet",
                "--bare",
                source.to_str().expect("UTF-8"),
                bare.to_str().expect("UTF-8"),
            ],
        ] {
            let output = child::run(site.command().args(args));
            assert!(output.status.success(), "{args:?}: {output:?}");
        }

        // git's protocol crosses the stand-in's pipes both ways, with v2 and v0.
        let ssh = site.ssh_command("person/ada");
        for version in ["2", "0"] {
            let mut command = site.command();
            command
                .env("GIT_SSH_COMMAND", &ssh)
                .args(["-c", &format!("protocol.version={version}"), "ls-remote"])
                .arg(site.ssh_url(STORE));
            let output = child::run(&mut command);
            let listed = String::from_utf8_lossy(&output.stdout);
            assert!(output.status.success(), "v{version}: {output:?}");
            assert!(listed.contains("refs/heads/main"), "v{version}: {listed}");
        }
    }

    #[test]
    fn cgi_gets_remote_user_and_protocol() {
        let work = tempfile::tempdir().expect("a temp dir");
        let record = work.path().join("record");
        let body = work.path().join("body");
        let site = scripted(
            work.path(),
            &format!(
                "{{ printf 'arg=%s\\n' \"$@\"; env; }} > '{}'\ncat > '{}'\n\
                 printf 'Content-Type: text/plain\\r\\n\\r\\nrecorded'\n",
                record.display(),
                body.display(),
            ),
        );
        let auth = HttpAuth::Basic {
            user: "ada".to_owned(),
            token: "token".to_owned(),
        };
        let http = site.http(HttpIdentity::UserFamily("person".to_owned()), auth);

        // 1. A POST with a query, the protocol header, Basic auth and a body.
        let raw = format!(
            "POST /{STORE}.git/git-upload-pack?x=1 HTTP/1.1\r\nHost: {}\r\n\
             Authorization: Basic {}\r\nGit-Protocol: version=2\r\n\
             Content-Type: application/x-git-upload-pack-request\r\nContent-Length: 3\r\n\r\nabc",
            http.host(),
            base64(b"ada:token"),
        );
        let (status, answer) = exchange(&http, raw.as_bytes());
        assert_eq!((status, answer.as_str()), (200, "recorded"));

        // 2. The arguments of the CGI entry.
        let record = std::fs::read_to_string(&record).expect("the CGI ran");
        let args: Vec<&str> = record
            .lines()
            .filter_map(|line| line.strip_prefix("arg="))
            .collect();
        let root = site.root();
        let root = root.to_str().expect("UTF-8");
        let expected = ["serve", "--cgi", "--root", root, "--user-family", "person"];
        assert_eq!(args, expected, "{record}");

        // 3. The RFC 3875 metavariables, and the body on stdin.
        let env = variables(&record);
        for (name, value) in [
            ("REMOTE_USER", "ada"),
            ("HTTP_GIT_PROTOCOL", "version=2"),
            ("PATH_INFO", "/acme.git/git-upload-pack"),
            ("QUERY_STRING", "x=1"),
            ("CONTENT_LENGTH", "3"),
            ("REQUEST_METHOD", "POST"),
            ("CONTENT_TYPE", "application/x-git-upload-pack-request"),
        ] {
            assert_eq!(env.get(name).map(String::as_str), Some(value), "{record}");
        }
        assert!(!env.contains_key("GIT_PROTOCOL"), "{record}");
        let stdin = std::fs::read_to_string(&body).expect("the body was recorded");
        assert_eq!(stdin, "abc");
    }

    #[test]
    fn cgi_status_header_sets_the_status() {
        let work = tempfile::tempdir().expect("a temp dir");
        let site = scripted(
            work.path(),
            "printf 'Status: 403\\r\\nContent-Type: text/plain\\r\\n\\r\\nE_AUTH: no identity'\n",
        );
        let http = site.http(
            HttpIdentity::Fixed("person/ada".to_owned()),
            HttpAuth::Anonymous,
        );
        let (status, body) = get(&http);
        assert_eq!((status, body.as_str()), (403, "E_AUTH: no identity"));
        assert_eq!(http.requests()[0].status(), 403);
    }

    #[test]
    fn cgi_without_headers_is_500() {
        let work = tempfile::tempdir().expect("a temp dir");
        let site = scripted(
            work.path(),
            "printf 'no header block'\necho 'the reason' >&2\n",
        );
        let http = site.http(
            HttpIdentity::Fixed("person/ada".to_owned()),
            HttpAuth::Anonymous,
        );

        // Answered, not hung: 500 with the script's stderr.
        let (status, body) = get(&http);
        assert_eq!((status, body.as_str()), (500, "the reason\n"));
    }
}
