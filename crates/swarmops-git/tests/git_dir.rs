//! The files the source reads in a repository's git directory: `HEAD`, `config`, `commondir`,
//! `gitdir`, `config.worktree`, `packed-refs`, `objects/info/alternates` (and those of the
//! object directories it names), and every ref. A FIFO, a socket or a device among them, or a
//! symlink to one, is refused before anything opens it: an extracted archive can hold one, the
//! open of a FIFO blocks until something writes to it, and a device's read can block or never
//! end. Files the source never reads are not looked at. Each open runs on a thread of its own
//! with a deadline, so that a regression fails the test instead of hanging it. Unix alone has
//! FIFOs, sockets and devices.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::symlink;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use swarmops_git::{
    GitOid, GitSink, GitSource, GixSink, GixSource, RawKind, SinkError, SourceError, Unsupported,
};
use swarmops_testkit::git::{self, GitFixture};

/// How long an open and a lookup may take: they read a few small files. A regression that
/// blocks on a FIFO fails here instead of hanging the test.
const DEADLINE: Duration = Duration::from_secs(10);

fn repo(fixture: &GitFixture) -> &Path {
    fixture
        .repo
        .as_deref()
        .expect("the fixture holds a repository")
}

/// A FIFO at `path`, in place of what was there: `mkfifo`, as the store's tests make one (the
/// workspace forbids `unsafe`, and so `libc::mkfifo`).
fn fifo(path: &Path) {
    let _ = fs::remove_file(path);
    let made = Command::new("mkfifo")
        .arg(path)
        .status()
        .expect("run mkfifo");
    assert!(made.success(), "mkfifo {}", path.display());
}

/// A socket at `path`, in place of what was there; it stays a socket file once its listener is
/// dropped.
fn socket(path: &Path) {
    let _ = fs::remove_file(path);
    drop(UnixListener::bind(path).unwrap_or_else(|error| panic!("{}: {error}", path.display())));
}

/// A symlink at `path` to `target`, in place of what was there.
fn link(target: &Path, path: &Path) {
    let _ = fs::remove_file(path);
    symlink(target, path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
}

/// `run` on a thread of its own: what it gave, or `None` when it was still running at the
/// deadline (blocked; the thread is left) or panicked.
fn within<T: Send + 'static>(run: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (sent, got) = mpsc::channel();
    thread::spawn(move || {
        let _ = sent.send(run());
    });
    got.recv_timeout(DEADLINE).ok()
}

/// The repository at `path` opened and `name` resolved in it, [`within`] the deadline.
fn open_and_resolve(path: &Path, name: &str) -> Option<Result<(), SourceError>> {
    let (path, name) = (path.to_owned(), name.to_owned());
    within(move || {
        let opened = GixSource::open(&path).and_then(|source| source.resolve(&name));
        opened.map(drop)
    })
}

/// A fresh copy of the bare `linear` fixture, with `HEAD` naming `main`, whose ref is packed.
fn linear() -> GitFixture {
    git::fixture("linear")
}

/// Turns on `extensions.worktreeConfig`, under which gix reads `config.worktree` too.
fn worktree_config(git_dir: &Path) {
    let config = git_dir.join("config");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str("[core]\n\trepositoryformatversion = 1\n[extensions]\n\tworktreeConfig = true\n");
    fs::write(&config, text).unwrap();
}

/// A case: what it is, the repository it opens (its fixtures kept alive), the name it resolves,
/// and the file it is refused for.
struct Case {
    what: &'static str,
    open: PathBuf,
    resolve: &'static str,
    file: String,
    _fixtures: Vec<GitFixture>,
}

impl Case {
    /// The case `what` on a `linear` fixture changed by `change`, opened at its git directory and
    /// resolving `HEAD`, refused for `file`.
    fn bare(what: &'static str, file: &str, change: impl FnOnce(&Path)) -> Case {
        let fixture = linear();
        change(repo(&fixture));
        Case {
            what,
            open: repo(&fixture).to_owned(),
            resolve: "HEAD",
            file: file.to_owned(),
            _fixtures: vec![fixture],
        }
    }
}

/// The cases in a bare repository: each file the open and a lookup of `HEAD` read, made a FIFO,
/// a socket or a device (or a symlink to one); and a pseudo-ref, which a lookup of its name reads
/// at the top of the git directory.
fn bare_cases() -> Vec<Case> {
    let mut cases = vec![
        Case::bare("a FIFO config", "config", |dir| fifo(&dir.join("config"))),
        Case::bare("a FIFO HEAD", "HEAD", |dir| fifo(&dir.join("HEAD"))),
        Case::bare("a FIFO commondir", "commondir", |dir| {
            fifo(&dir.join("commondir"));
        }),
        // gix reads `gitdir` beside a `commondir` that reads.
        Case::bare("a FIFO gitdir", "gitdir", |dir| {
            fs::write(dir.join("commondir"), ".\n").unwrap();
            fifo(&dir.join("gitdir"));
        }),
        Case::bare("a FIFO config.worktree", "config.worktree", |dir| {
            worktree_config(dir);
            fifo(&dir.join("config.worktree"));
        }),
        Case::bare("a FIFO packed-refs", "packed-refs", |dir| {
            fifo(&dir.join("packed-refs"));
        }),
        Case::bare(
            "a FIFO objects/info/alternates",
            "objects/info/alternates",
            |dir| fifo(&dir.join("objects/info/alternates")),
        ),
        // `HEAD` names `main`, whose loose file is read before `packed-refs`.
        Case::bare("a FIFO main", "refs/heads/main", |dir| {
            fifo(&dir.join("refs/heads/main"));
        }),
        Case::bare("a socket config", "config", |dir| {
            socket(&dir.join("config"));
        }),
        Case::bare("config, a symlink to a device", "config", |dir| {
            link(Path::new("/dev/null"), &dir.join("config"));
        }),
        Case::bare("main, a symlink to a FIFO", "refs/heads/main", |dir| {
            fifo(&dir.join("elsewhere"));
            link(&dir.join("elsewhere"), &dir.join("refs/heads/main"));
        }),
    ];
    let mut fetch_head = Case::bare("a FIFO FETCH_HEAD", "FETCH_HEAD", |dir| {
        fifo(&dir.join("FETCH_HEAD"));
    });
    fetch_head.resolve = "FETCH_HEAD";
    cases.push(fetch_head);
    cases
}

/// The cases laid out otherwise: a work tree whose `.git` is the git directory, one whose `.git`
/// file names it, and an alternate object directory elsewhere whose own alternates file is a
/// FIFO (gix reads the chain as the repository opens; the refusal names it by its path).
fn laid_out_cases() -> Vec<Case> {
    let mut cases = Vec::new();

    let fixture = linear();
    let work = fixture.dir.join("work");
    fs::create_dir(&work).unwrap();
    fs::rename(repo(&fixture), work.join(".git")).unwrap();
    fifo(&work.join(".git/config"));
    cases.push(Case {
        what: "a FIFO config in a work tree's .git",
        open: work,
        resolve: "HEAD",
        file: "config".to_owned(),
        _fixtures: vec![fixture],
    });

    let fixture = linear();
    let work = fixture.dir.join("work");
    fs::create_dir(&work).unwrap();
    let pointer = format!("gitdir: {}\n", repo(&fixture).display());
    fs::write(work.join(".git"), pointer).unwrap();
    fifo(&repo(&fixture).join("HEAD"));
    cases.push(Case {
        what: "a FIFO HEAD behind a .git file",
        open: work,
        resolve: "HEAD",
        file: "HEAD".to_owned(),
        _fixtures: vec![fixture],
    });

    let (fixture, other) = (linear(), linear());
    let objects = repo(&other).join("objects");
    let listed = format!("{}\n", objects.display());
    fs::write(repo(&fixture).join("objects/info/alternates"), listed).unwrap();
    let alternates = objects.join("info/alternates");
    fifo(&alternates);
    cases.push(Case {
        what: "a FIFO in an alternate's alternates",
        open: repo(&fixture).to_owned(),
        resolve: "HEAD",
        file: alternates.display().to_string(),
        _fixtures: vec![fixture, other],
    });
    cases
}

/// Every file the open and a lookup read, made a FIFO, a socket or a device (or a symlink to
/// one), is refused for that file ([`bare_cases`], [`laid_out_cases`]). The cases run at once and
/// each is waited for, so that a failure lists every case that blocked or opened, and the cases
/// that block cost one deadline together.
#[test]
fn a_special_file_where_the_source_reads_is_refused_before_it_is_opened() {
    let cases: Vec<Case> = bare_cases().into_iter().chain(laid_out_cases()).collect();
    let running: Vec<_> = cases
        .iter()
        .map(|case| {
            let (sent, got) = mpsc::channel();
            let (path, name) = (case.open.clone(), case.resolve);
            thread::spawn(move || {
                let _ = sent.send(open_and_resolve(&path, name));
            });
            got
        })
        .collect();
    let mut wrong = Vec::new();
    for (case, got) in cases.iter().zip(running) {
        let refused = Err(SourceError::Unsupported(Unsupported::SpecialFile(
            case.file.clone(),
        )));
        match got.recv().expect("a case's thread answers") {
            Some(result) if result == refused => {}
            Some(result) => wrong.push(format!("{}: {result:?}", case.what)),
            None => wrong.push(format!("{}: blocked past {DEADLINE:?}", case.what)),
        }
    }
    assert!(wrong.is_empty(), "not refused:\n{}", wrong.join("\n"));
}

/// A `config` that is a symlink to `/dev/zero`, which reads zeros without end, is refused
/// before a byte of it is read: the refusal comes at once, where a read of it would grow until
/// memory ran out.
#[test]
fn a_config_symlinked_to_dev_zero_is_refused_unread() {
    let fixture = linear();
    link(Path::new("/dev/zero"), &repo(&fixture).join("config"));
    let (sent, got) = mpsc::channel();
    let path = repo(&fixture).to_owned();
    thread::spawn(move || {
        let _ = sent.send(GixSource::open(&path).map(drop));
    });
    // A short deadline: past it, a regression has been filling memory for that long.
    let opened = got.recv_timeout(Duration::from_secs(2));
    assert_eq!(
        opened,
        Ok(Err(SourceError::Unsupported(Unsupported::SpecialFile(
            "config".to_owned()
        ))))
    );
}

/// What the source never reads is not looked at, and a symlink to a regular file or to a
/// directory is followed: a repository with a socket where git's file-system monitor keeps
/// one, a `config` that is a symlink to a regular file and a `refs` that is a symlink to a
/// directory (as `git-new-workdir` makes them), and a dangling symlink among its refs opens, and
/// `HEAD` resolves.
#[test]
fn files_the_source_does_not_read_and_symlinks_to_files_are_taken() {
    let fixture = linear();
    let dir = repo(&fixture);
    socket(&dir.join("fsmonitor--daemon.ipc"));
    fs::rename(dir.join("config"), fixture.dir.join("config")).unwrap();
    link(&fixture.dir.join("config"), &dir.join("config"));
    fs::rename(dir.join("refs"), fixture.dir.join("refs")).unwrap();
    link(&fixture.dir.join("refs"), &dir.join("refs"));
    link(
        &fixture.dir.join("nothing"),
        &dir.join("refs/heads/dangling"),
    );
    assert_eq!(open_and_resolve(dir, "HEAD"), Some(Ok(())));
}

/// The export opens its repository as the source does, and so refuses the same files
/// ([`SinkError::Unsupported`]); and its own read of a loose object passes a FIFO over where the
/// object's loose file would be, as gix's lookups do, and finds the object in its pack.
#[test]
fn the_export_refuses_them_too_and_reads_no_fifo_for_an_object() {
    let fixture = linear();
    fifo(&repo(&fixture).join("config"));
    let path = repo(&fixture).to_owned();
    let refused = SinkError::Unsupported(Unsupported::SpecialFile("config".to_owned()));
    assert_eq!(
        within(move || GixSink::open(&path).map(drop)),
        Some(Err(refused))
    );

    let fixture = linear();
    let commit = &fixture.manifest.as_ref().expect("a manifest").commits[0];
    let (hex, raw) = (commit.oid.clone(), commit.raw.clone());
    let fan_out = repo(&fixture).join("objects").join(&hex[..2]);
    fs::create_dir_all(&fan_out).unwrap();
    fifo(&fan_out.join(&hex[2..]));
    let path = repo(&fixture).to_owned();
    let id: GitOid = hex.parse().unwrap();
    let read = within(move || GixSink::open(&path).and_then(|sink| sink.read(id)));
    assert!(read == Some(Ok((RawKind::Commit, raw))), "{read:?}");
}
