//! Host git as the server's child (M14 design §0.1): `upload-pack`, `receive-pack` or
//! `http-backend` on `serve.git`, built by [`GitCli`]'s one builder, its configuration in the
//! environment.
//!
//! ```text
//! client ══ stdin/stdout ══▶ git <service> (GIT_DIR = serve.git, config in GIT_CONFIG_*)
//!                                 │ stderr        │ a fetch's pack: PACK_OBJECTS_HOOK
//!                                 ▼               ▼
//!                       the store's paths masked  serve --hook pack-objects
//!                       ──▶ serve's stderr        ├─ check_tips, check_wants on Unknown
//!                                                 └─ pack_objects on a miss
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead as _, BufReader, Read, Write};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread;

use super::repo::{PACK_OBJECTS_HOOK, ServeError, ServeRepo};
use crate::GixSource;
use crate::oid::{GitOid, RawKind};
use crate::source::GitSource as _;
use crate::transport::GitCli;

/// What `upload-pack` serves: no partial clone filter (M14 design §1), and no object that no
/// served ref reaches, so that a refused push's objects are never fetched.
const UPLOAD_CONFIG: [(&str, &str); 2] = [
    ("uploadpack.allowFilter", "false"),
    ("uploadpack.allowAnySHA1InWant", "false"),
];

/// What `receive-pack` takes (M14 design §6): every command handed to the `proc-receive` hook,
/// after the `pre-receive` hook, which git always runs, accepted the push (M16 design §7), every
/// object checked, and no atomic push, push options or shallow update advertised.
const RECEIVE_CONFIG: [(&str, &str); 5] = [
    ("receive.procReceiveRefs", "refs/"),
    ("receive.fsckObjects", "true"),
    ("receive.advertiseAtomic", "false"),
    ("receive.advertisePushOptions", "false"),
    ("receive.shallowUpdate", "false"),
];

/// What `http-backend` adds to both: a push without `REMOTE_USER` reaches the hook, where the
/// access file alone decides (M14 design D3).
const HTTP_CONFIG: [(&str, &str); 1] = [("http.receivepack", "true")];

/// The setting that points git at the serving hooks.
const HOOKS_PATH: &str = "core.hooksPath";

/// The setting that hands a fetch's `pack-objects` to [`PACK_OBJECTS_HOOK`]: git honours it in
/// protected configuration alone, which the environment's is.
const PACK_HOOK: &str = "uploadpack.packObjectsHook";

/// What `cat-file --batch-check` prints for a wanted object that may be sent.
const COMMIT: &str = "commit";

/// The variable that carries the client's protocol.
const GIT_PROTOCOL: &str = "GIT_PROTOCOL";

/// What a masked store path reads as in git's messages.
const STORE_MASK: &str = "<store>";

/// The store's directory above `serve.git`: `<store>/git/serve.git`.
const STORE_DEPTH: usize = 2;

/// The git service a connection runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Service {
    /// `git upload-pack`: fetch and clone.
    UploadPack,
    /// `git receive-pack`: push.
    ReceivePack,
    /// `git http-backend`: either, over smart HTTP.
    HttpBackend,
}

/// What the child gets besides its configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChildEnv {
    /// The client's `GIT_PROTOCOL`, passed on as it came.
    pub git_protocol: Option<OsString>,
    /// Variables the child gets: the hook's `SWARMOPS_SERVE_*`, a CGI request's metavariables.
    pub vars: BTreeMap<String, OsString>,
    /// Configuration the caller adds after the service's own, which it wins over:
    /// `receive.maxInputSize`.
    pub config: Vec<(String, OsString)>,
}

impl ChildEnv {
    /// No protocol, no variables and no added configuration.
    pub fn new() -> ChildEnv {
        ChildEnv::default()
    }
}

/// Runs `service` on `repo` with `env`, its stdin and stdout the connection's; its exit. git's
/// stderr reaches this process's with the store's path masked.
pub fn run(
    git: &GitCli,
    service: Service,
    repo: &ServeRepo,
    env: &ChildEnv,
) -> Result<ExitStatus, ServeError> {
    // 1. The configuration: the service's, the hooks, then the caller's.
    let hooks = OsString::from(repo.hooks_dir().as_str());
    let fixed = service_config(service)
        .into_iter()
        .map(|(key, value)| (key, OsStr::new(value)));
    let pack_hook = pack_hook(service).map(|hook| (PACK_HOOK, OsStr::new(hook)));
    let added = (env.config.iter()).map(|(key, value)| (key.as_str(), value.as_os_str()));
    let config: Vec<(&str, &OsStr)> = fixed
        .chain(pack_hook)
        .chain([(HOOKS_PATH, hooks.as_os_str())])
        .chain(added)
        .collect();

    // 2. The command: the service on `.` (the working directory is serve.git), the client's
    //    protocol or none, the caller's variables, the connection's stdin and stdout.
    let mut command = git.serve_command(repo.git_dir().as_std_path(), &config);
    command.args(service_args(service));
    match &env.git_protocol {
        Some(protocol) => command.env(GIT_PROTOCOL, protocol),
        None => command.env_remove(GIT_PROTOCOL),
    };
    command
        .envs(&env.vars)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped());

    // 3. Run it to its end, its stderr masked on the way.
    let mut child = command.spawn().map_err(|e| failed(&e))?;
    if let Some(stderr) = child.stderr.take() {
        copy_masked(stderr, &mut io::stderr(), &masks(repo));
    }
    child.wait().map_err(|e| failed(&e))
}

/// The configuration `service` runs with, before the hooks path and the caller's.
fn service_config(service: Service) -> Vec<(&'static str, &'static str)> {
    match service {
        Service::UploadPack => UPLOAD_CONFIG.to_vec(),
        Service::ReceivePack => RECEIVE_CONFIG.to_vec(),
        Service::HttpBackend => {
            [&UPLOAD_CONFIG[..], &RECEIVE_CONFIG[..], &HTTP_CONFIG[..]].concat()
        }
    }
}

/// The pack-objects hook `service` runs with: every service that answers a fetch.
fn pack_hook(service: Service) -> Option<&'static str> {
    match service {
        Service::UploadPack | Service::HttpBackend => Some(PACK_OBJECTS_HOOK),
        Service::ReceivePack => None,
    }
}

/// Whether the objects a fetch wants may be sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Wants {
    /// Each is a commit a ref of `serve.git` is or reaches.
    Served,
    /// One is not: another type, an object no ref reaches, or none at all.
    NotServed,
    /// Not known in process: [`check_wants`] decides.
    Unknown,
}

/// Whether each of `wants` is a tip of `repo` on a commit (the M17 closure design §3.2), read in
/// process: `serve.git`'s refs, then each want's object header. [`Wants::Served`] when it is,
/// else [`Wants::Unknown`], for [`check_wants`] to decide: none, a want no ref names, one on
/// another type, an object that cannot be read.
pub fn check_tips(repo: &ServeRepo, wants: &[GitOid]) -> Result<Wants, ServeError> {
    // 1. Each a tip: none is no tip.
    let tips: BTreeSet<GitOid> = repo.refs()?.into_values().collect();
    if wants.is_empty() || !wants.iter().all(|want| tips.contains(want)) {
        return Ok(Wants::Unknown);
    }

    // 2. Each a commit, by its header; a repository or an object not read is not known.
    let Ok(source) = GixSource::open(repo.git_dir().as_std_path()) else {
        return Ok(Wants::Unknown);
    };
    let commits = wants
        .iter()
        .all(|want| matches!(source.header(*want), Ok((RawKind::Commit, _))));
    if !commits {
        return Ok(Wants::Unknown);
    }
    Ok(Wants::Served)
}

/// Whether each of `wants` is a commit a ref of `repo` is or reaches (the M17 design §5 step
/// 2): their types from one `cat-file --batch-check`, then one `rev-list --count`, from the
/// wants and not from any ref, that must count none. A git that fails says
/// [`Wants::NotServed`].
pub fn check_wants(git: &GitCli, repo: &ServeRepo, wants: &[GitOid]) -> Result<Wants, ServeError> {
    if wants.is_empty() {
        return Ok(Wants::NotServed);
    }
    let listed: String = wants.iter().map(|oid| oid.to_string() + "\n").collect();

    // 1. Each a commit.
    let mut cat_file = git.serve_command(repo.git_dir().as_std_path(), &[]);
    cat_file.args(["cat-file", "--batch-check=%(objecttype)"]);
    let typed = with_input(cat_file, listed.clone().into_bytes())?;
    let commits = String::from_utf8_lossy(&typed.stdout)
        .lines()
        .filter(|line| *line == COMMIT)
        .count();
    if !typed.status.success() || commits != wants.len() {
        return Ok(Wants::NotServed);
    }

    // 2. None past what the refs reach: the wants from stdin, every ref after --not.
    let mut reach = git.serve_command(repo.git_dir().as_std_path(), &[]);
    reach.args(["rev-list", "--count", "--stdin", "--not", "--all"]);
    let counted = with_input(reach, listed.into_bytes())?;
    let count = String::from_utf8_lossy(&counted.stdout).trim().to_owned();
    if counted.status.success() && count == "0" {
        return Ok(Wants::Served);
    }
    Ok(Wants::NotServed)
}

/// git's `pack-objects` on `repo`, started as `upload-pack` would have run it, `args` the line
/// after `git` (`[--shallow-file <file>] pack-objects …`): stdin and stdout piped to the caller,
/// stderr git's progress, which `upload-pack` reads.
pub fn pack_objects(
    git: &GitCli,
    repo: &ServeRepo,
    args: &[OsString],
) -> Result<Child, ServeError> {
    let mut command = git.serve_command(repo.git_dir().as_std_path(), &[]);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    command.spawn().map_err(|e| failed(&e))
}

/// Runs `command` with `input` on its stdin, written beside the reading of its output so that
/// neither pipe blocks the other; its output, its stderr dropped.
fn with_input(mut command: Command, input: Vec<u8>) -> Result<Output, ServeError> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|e| failed(&e))?;

    // A git that exits early closes its stdin: the write's error is its exit's to say.
    let stdin = child.stdin.take();
    let writer = thread::spawn(move || {
        if let Some(mut stdin) = stdin {
            let _ = stdin.write_all(&input);
        }
    });
    let output = child.wait_with_output().map_err(|e| failed(&e));
    let _ = writer.join();
    output
}

/// git's arguments for `service`. `upload-pack` and `receive-pack` take the repository, `.`,
/// strictly (no `.git` appended); `http-backend` reads its request from the environment.
fn service_args(service: Service) -> &'static [&'static str] {
    match service {
        Service::UploadPack => &["upload-pack", "--strict", "--end-of-options", "."],
        Service::ReceivePack => &["receive-pack", "--end-of-options", "."],
        Service::HttpBackend => &["http-backend"],
    }
}

/// The texts of the store's path git may print: as `serve.git` names it, and canonical.
fn masks(repo: &ServeRepo) -> Vec<String> {
    let Some(store) = repo.git_dir().ancestors().nth(STORE_DEPTH) else {
        return Vec::new();
    };
    let mut masks = vec![store.as_str().to_owned()];
    if let Ok(canonical) = store.canonicalize_utf8() {
        masks.push(canonical.into_string());
    }
    masks.retain(|mask| mask.len() > 1);
    masks.sort_by_key(|mask| std::cmp::Reverse(mask.len()));
    masks
}

/// Copies `from` to `to` line by line, each of `masks` replaced by [`STORE_MASK`]. A write that
/// fails ends the copy's writing, not its reading: git is never blocked on a full pipe.
fn copy_masked(from: impl Read, to: &mut impl Write, masks: &[String]) {
    let mut from = BufReader::new(from);
    let mut line = Vec::new();
    let mut writing = true;
    loop {
        line.clear();
        match from.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if !writing {
            continue;
        }
        let text = masks
            .iter()
            .fold(String::from_utf8_lossy(&line).into_owned(), |text, mask| {
                text.replace(mask.as_str(), STORE_MASK)
            });
        writing = to
            .write_all(text.as_bytes())
            .and_then(|()| to.flush())
            .is_ok();
    }
}

/// The error of a git child that did not start or could not be waited for.
fn failed(e: &io::Error) -> ServeError {
    ServeError::Io {
        what: "serve.git".to_owned(),
        reason: format!("running git: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Service, Wants, check_tips, copy_masked, service_args, service_config};
    use crate::serve::repo::ServeRepo;
    use crate::{GitSink as _, GixSink, RawKind};

    /// Each service's configuration: upload-pack's for reading, receive-pack's for pushing,
    /// both and `http.receivepack` behind a web server.
    #[test]
    fn each_service_gets_its_configuration() {
        let keys = |service| -> Vec<&str> {
            service_config(service)
                .into_iter()
                .map(|(key, _)| key)
                .collect()
        };
        assert_eq!(
            keys(Service::UploadPack),
            ["uploadpack.allowFilter", "uploadpack.allowAnySHA1InWant"]
        );
        let receive = keys(Service::ReceivePack);
        assert!(receive.contains(&"receive.procReceiveRefs"), "{receive:?}");
        assert!(receive.contains(&"receive.advertiseAtomic"), "{receive:?}");
        let http = keys(Service::HttpBackend);
        for key in keys(Service::UploadPack).into_iter().chain(receive) {
            assert!(http.contains(&key), "{key}: {http:?}");
        }
        assert!(http.contains(&"http.receivepack"), "{http:?}");
        assert_eq!(service_args(Service::UploadPack)[0], "upload-pack");
    }

    /// git's stderr reaches the client with the store's path masked, line by line, and a last
    /// line without its newline kept.
    #[test]
    fn stderr_is_masked() {
        let masks = ["/srv/so/acme".to_owned()];
        let input = b"fatal: /srv/so/acme/git/serve.git: broken\nlast /srv/so/acme" as &[u8];
        let mut out = Vec::new();
        copy_masked(input, &mut out, &masks);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "fatal: <store>/git/serve.git: broken\nlast <store>"
        );
    }

    /// Only wants that are each a served tip on a commit are served in process; anything else,
    /// a tip on a tree, a commit no ref names, none at all, is left to git's check.
    #[test]
    fn only_commit_tips_are_served() {
        // 1. serve.git with a commit on main, its tree on another ref, and a commit no ref names.
        let root = tempfile::tempdir().unwrap();
        let repo = ServeRepo::open(camino::Utf8Path::from_path(root.path()).unwrap()).unwrap();
        let mut sink = GixSink::open(repo.git_dir().as_std_path()).unwrap();
        let tree = sink.write_object(RawKind::Tree, b"").unwrap();
        let commit = |message: &str| {
            let bytes = format!(
                "tree {tree}\nauthor A <a@b> 0 +0000\ncommitter A <a@b> 0 +0000\n\n{message}\n"
            );
            bytes.into_bytes()
        };
        let tip = sink.write_object(RawKind::Commit, &commit("tip")).unwrap();
        let loose = sink
            .write_object(RawKind::Commit, &commit("loose"))
            .unwrap();
        sink.sync(&[tree, tip, loose]).unwrap();
        let lock = repo.lock(Duration::ZERO).unwrap();
        for (name, oid) in [("refs/heads/main", tip), ("refs/heads/tree", tree)] {
            repo.set_ref(&lock, name, None, Some(oid)).unwrap();
        }

        // 2. The tip alone is served; the rest are unknown.
        assert_eq!(check_tips(&repo, &[tip]).unwrap(), Wants::Served);
        for wants in [&[tree][..], &[loose], &[], &[tip, tree]] {
            assert_eq!(
                check_tips(&repo, wants).unwrap(),
                Wants::Unknown,
                "{wants:?}"
            );
        }
    }
}
