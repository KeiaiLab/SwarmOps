//! The git fixtures: small repositories that real git made, committed as archives beside the
//! scripts that made them, so that the git import's tests read what git itself writes without
//! running git.
//!
//! A fixture is a POSIX sh script, `git-fixtures/scripts/<name>.sh`, run as
//! `sh <name>.sh <out>` in a pinned environment: nothing of the caller's but `PATH` (and
//! `TMPDIR`), no system or global git configuration (`GIT_CONFIG_NOSYSTEM=1`,
//! `GIT_CONFIG_GLOBAL=/dev/null`), an empty `HOME`, `TZ=UTC`, `LC_ALL=C`, and a fixed author and
//! committer (`Fixture Author <author@fixture.invalid>`,
//! `Fixture Committer <committer@fixture.invalid>`). It sources `scripts/lib.sh`, whose helpers
//! are `tick` (the author and committer dates, from `1700000000 +0000`, 60 s a commit),
//! `commit_all <message>`, `craft <type> <file>` (`git hash-object -t <type> -w --literally`,
//! for objects git would not make; a `<file>` of `-` is stdin), `entry <mode> <name> <id>` (one
//! tree entry's bytes, for a crafted tree), `idents` (the author and committer lines git writes
//! at this tick, for a crafted commit) and `finish [--keep-loose]` (the work tree's `.git`
//! becomes `<out>/repo.git`, bare, holding only what its refs reach; then, unless
//! `--keep-loose`, `git -c pack.threads=1 repack -adq` and `git pack-refs --all`).
//!
//! Whatever the script leaves in `<out>` is the fixture, archived as `git-fixtures/<name>.tar`
//! without what git keeps of a repository for a work tree or a person (under `repo.git/`:
//! `hooks/`, `logs/`, `index`, `COMMIT_EDITMSG`, `description`, `FETCH_HEAD`, `ORIG_HEAD`). A
//! script that leaves `<out>/repo.git` also gets `<out>/manifest.json`, the [`Manifest`] git's
//! plumbing reads from it, which the tests take as the oracle. The archives are deterministic
//! (entries sorted by path, mtime 0, uid and gid 0, no user or group name, mode 0644 or 0755),
//! so a rebuild by the same git rewrites them byte for byte.
//!
//! A test takes a fixture with [`fixture`], extracted into a fresh temporary directory. Only two
//! switches run git, each on purpose:
//!
//! - `just git-fixtures` rebuilds every archive ([`regenerate`], through the ignored test
//!   `regenerate_git_fixtures` and [`REGEN_ENV`]) with the git `git-fixtures/GIT_VERSION`
//!   declares, and refuses any other unless [`ANY_GIT_ENV`] is `1`.
//! - The version matrix rebuilds every fixture with the git [`GIT_ENV`] names into the
//!   directory [`CHECK_ENV`] names and compares each manifest with its archive's ([`check`],
//!   through the ignored test `check_git_fixtures`); [`DIR_ENV`] then makes [`fixture`] take
//!   those fixtures instead of the archives.
//!
//! A new fixture, for the git import or for another milestone's corpus, is one more script and
//! the archive `just git-fixtures` writes for it.
//!
//! A script whose first lines (the comment lines it starts with) say `# needs git X.Y` is made by
//! that release of git or a later one only: an older git skips it and says so. While the
//! declared git is older, the script has no archive, and the fixtures at hand lack it: [`names`]
//! leaves it out, and [`skipped`] says why, for a test of it to ask first. `reftable` is one:
//! the version matrix's newest git makes it.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

use crate::child;

/// Set (to `1`), it makes the ignored test `regenerate_git_fixtures` rewrite every archive in
/// `git-fixtures/` ([`regenerate`]); unset, that test writes nothing.
pub const REGEN_ENV: &str = "SWARMOPS_REGEN_GIT_FIXTURES";

/// A directory: the ignored test `check_git_fixtures` rebuilds every fixture into it and
/// compares the manifests with the archives' ([`check`]); unset, that test does nothing.
pub const CHECK_ENV: &str = "SWARMOPS_GIT_FIXTURES_CHECK";

/// A directory of fixtures [`check`] built: [`fixture`] copies `<dir>/<name>/` instead of
/// extracting `<name>.tar`, and `<dir>/GIT_VERSION` names the git that built them ([`skipped`]).
pub const DIR_ENV: &str = "SWARMOPS_GIT_FIXTURES_DIR";

/// The absolute path of the git binary to run, a file named `git` (its directory goes first on
/// the scripts' `PATH`); unset, `git` on `PATH`.
pub const GIT_ENV: &str = "SWARMOPS_GIT";

/// `1` lets [`regenerate`] rebuild the archives with a git other than the declared one.
pub const ANY_GIT_ENV: &str = "SWARMOPS_GIT_FIXTURES_ANY_GIT";

/// The environment a script and every git run of the harness get, beside `PATH`, an empty
/// `HOME` and the caller's `TMPDIR`.
const PINNED: [(&str, &str); 8] = [
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("TZ", "UTC"),
    ("LC_ALL", "C"),
    ("GIT_AUTHOR_NAME", "Fixture Author"),
    ("GIT_AUTHOR_EMAIL", "author@fixture.invalid"),
    ("GIT_COMMITTER_NAME", "Fixture Committer"),
    ("GIT_COMMITTER_EMAIL", "committer@fixture.invalid"),
];

/// What a fixture leaves out of its `repo.git/`: what git keeps for a work tree or a person,
/// the reflogs and the sample hooks among them.
const LEFT_OUT: [&str; 7] = [
    "hooks",
    "logs",
    "index",
    "COMMIT_EDITMSG",
    "description",
    "FETCH_HEAD",
    "ORIG_HEAD",
];

/// A fixture in a temporary directory of its own, which lives as long as the fixture does.
#[derive(Debug)]
pub struct GitFixture {
    /// The directory the fixture's script left.
    pub dir: PathBuf,
    /// `<dir>/repo.git`, when the script made a repository.
    pub repo: Option<PathBuf>,
    /// That repository's manifest, `<dir>/manifest.json`.
    pub manifest: Option<Manifest>,
    /// The temporary directory that holds `dir`.
    _temp: TempDir,
}

/// What git's plumbing reads from a fixture's repository, with replacement objects off
/// (`GIT_NO_REPLACE_OBJECTS=1`): the oracle the git import's tests compare against. Its JSON
/// form, `manifest.json`, has its keys sorted and bytes in standard padded base64.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// The size of every blob the repository holds, by id (`cat-file --batch-all-objects`,
    /// which reads only what is there: a partial clone lacks blobs its trees name).
    pub blobs: BTreeMap<String, u64>,
    /// Every commit a ref or `HEAD` reaches, parents first
    /// (`rev-list --topo-order --reverse --parents --all`).
    pub commits: Vec<ManifestCommit>,
    /// What `git --version` printed for the git that read it.
    pub git_version: String,
    /// The ref `HEAD` names (`symbolic-ref -q HEAD`); `None` when `HEAD` is detached.
    pub head: Option<String>,
    /// The repository's object format, `sha1` or `sha256` (`rev-parse --show-object-format`).
    pub object_format: String,
    /// Every ref (`for-each-ref`), and the commit it peels to through annotated tags; a ref that
    /// peels to no commit (a tag of a tree or of a blob) is left out.
    pub refs: BTreeMap<String, String>,
    /// Whether the repository is a shallow clone (`rev-parse --is-shallow-repository`).
    pub shallow: bool,
    /// The bytes of every tree the repository holds, by id, as `cat-file` gives them.
    #[serde(with = "base64_values")]
    pub trees: BTreeMap<String, Vec<u8>>,
}

/// A commit of a [`Manifest`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestCommit {
    /// Its id.
    pub oid: String,
    /// Its parents' ids, in their order, as `rev-list --parents` gives them.
    pub parents: Vec<String>,
    /// Its bytes as `cat-file` gives them: the object without git's `commit <len>\0` header.
    #[serde(with = "base64_bytes")]
    pub raw: Vec<u8>,
    /// Its tree's id, from the first line of `raw`.
    pub tree: String,
}

/// A fixture, by name: its archive, `git-fixtures/<name>.tar`, extracted into a fresh temporary
/// directory; with [`DIR_ENV`] set, a copy of `<dir>/<name>/` instead. A name with no archive
/// fails the test, naming the script that builds it, and so does one the git that made the
/// fixtures is too old for ([`skipped`]).
pub fn fixture(name: &str) -> GitFixture {
    if let Some(why) = skipped(name) {
        panic!(
            "the git fixture {name} is not at hand: {why}; a test that takes it asks \
             git::skipped first"
        );
    }
    match std::env::var_os(DIR_ENV) {
        Some(dir) => copy_fixture(name, &Path::new(&dir).join(name)),
        None => extract_fixture(name),
    }
}

/// The fixtures [`fixture`] takes, by name: the scripts in `git-fixtures/scripts/` but `lib.sh`,
/// without their `.sh`, sorted, less those the git that made the fixtures is too old for
/// ([`skipped`]).
pub fn names() -> Vec<String> {
    scripts()
        .into_iter()
        .filter(|name| skipped(name).is_none())
        .collect()
}

/// Every fixture script's name: the scripts in `git-fixtures/scripts/` but `lib.sh`, without
/// their `.sh`, sorted.
fn scripts() -> Vec<String> {
    let scripts = scripts_dir();
    let mut names: Vec<String> = fs::read_dir(&scripts)
        .unwrap_or_else(|error| panic!("read {}: {error}", scripts.display()))
        .map(|entry| entry.expect("read a script's entry").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "sh"))
        .filter_map(|path| Some(path.file_stem()?.to_str()?.to_owned()))
        .filter(|name| name != "lib")
        .collect();
    names.sort();
    names
}

/// Why fixture `name` is not at hand: its script's first lines say `# needs git X.Y`, and the git
/// that made the fixtures [`fixture`] takes is older (the declared one, `git-fixtures/GIT_VERSION`,
/// for the archives; with [`DIR_ENV`] set, the one [`check`] ran). `None` when [`fixture`] takes
/// it. A test of such a fixture asks first, says why it is not there, and returns.
pub fn skipped(name: &str) -> Option<String> {
    // A name with no script is no fixture at all, which `fixture` says.
    let script = fs::read_to_string(scripts_dir().join(format!("{name}.sh"))).ok()?;
    let (version, whose) = fixtures_git();
    let why = too_old(&version, &script)?;
    Some(format!("{why} ({whose})"))
}

/// Rebuilds the archives of the fixtures `names` from their scripts, each in a fresh directory,
/// and writes them to `git-fixtures/`. Refuses unless the git it runs ([`GIT_ENV`]) prints the
/// line `git-fixtures/GIT_VERSION` holds, or [`ANY_GIT_ENV`] is `1`; fails the test with the
/// script's stderr when a script fails. A script that git is too old for (`# needs git X.Y`) is
/// skipped, and said to be.
pub fn regenerate(names: &[String]) {
    let git = Git::from_env();
    let version = git.version();
    let declared = declared_version();
    let any_git = std::env::var_os(ANY_GIT_ENV).is_some_and(|value| value == "1");
    assert!(
        version == declared || any_git,
        "{} prints {version:?}, but the git fixtures are made by {declared:?} \
         (git-fixtures/GIT_VERSION): set {GIT_ENV} to that git, or {ANY_GIT_ENV}=1 to make \
         them with this one all the same",
        git.program.display(),
    );
    let scratch = tempfile::tempdir().expect("create a temp dir");
    for name in names {
        if let Some(why) = too_old(&version, &script_text(name)) {
            println!("{name}: skipped: {why}");
            continue;
        }
        let out = scratch.path().join(name);
        if let Err(failure) = build(&git, name, &out) {
            panic!("{failure}");
        }
        let archive = archive_path(name);
        write_archive(&out, &archive);
        println!(
            "{name}: {} ({} bytes)",
            archive.display(),
            fs::metadata(&archive).expect("stat an archive").len(),
        );
    }
}

/// Rebuilds every fixture with the git it runs ([`GIT_ENV`]) into `<into>/<name>/`, archiving
/// nothing, writes that git's `git --version` line to `<into>/GIT_VERSION` (for [`DIR_ENV`]),
/// and compares each manifest field by field with its archive's (`git_version` aside): the
/// differences, one line each, a script that fails among them. A script that git is too old for
/// (`# needs git X.Y`) is skipped, and one the declared git is too old for, which has no archive,
/// is built and compared with nothing; each is said to be.
pub fn check(into: &Path) -> Vec<String> {
    let git = Git::from_env();
    let version = git.version();
    let declared = declared_version();
    fs::create_dir_all(into).unwrap_or_else(|error| panic!("create {}: {error}", into.display()));
    let marker = into.join("GIT_VERSION");
    fs::write(&marker, format!("{version}\n"))
        .unwrap_or_else(|error| panic!("write {}: {error}", marker.display()));
    let mut differences = Vec::new();
    for name in scripts() {
        let out = into.join(&name);
        if out.exists() {
            fs::remove_dir_all(&out)
                .unwrap_or_else(|error| panic!("remove {}: {error}", out.display()));
        }
        let script = script_text(&name);
        if let Some(why) = too_old(&version, &script) {
            println!("{name}: skipped: {why}");
            continue;
        }
        if let Err(failure) = build(&git, &name, &out) {
            differences.push(format!("{name}: {}", failure.replace('\n', " | ")));
            continue;
        }
        if let Some(why) = too_old(&declared, &script) {
            println!(
                "{name}: built, and compared with nothing: the declared git makes no archive of \
                 it ({why})"
            );
            continue;
        }
        if !archive_path(&name).exists() {
            differences.push(format!("{name}: {}", missing(&name)));
            continue;
        }
        let archived = extract_fixture(&name).manifest;
        let manifest = out.join("manifest.json");
        let rebuilt = manifest.exists().then(|| read_manifest(&manifest));
        compare_manifests(&mut differences, &name, archived.as_ref(), rebuilt.as_ref());
    }
    differences
}

/// `git-fixtures/`, beside this crate's `Cargo.toml`.
fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("git-fixtures")
}

/// `git-fixtures/scripts/`.
fn scripts_dir() -> PathBuf {
    fixtures_dir().join("scripts")
}

/// `git-fixtures/<name>.tar`.
fn archive_path(name: &str) -> PathBuf {
    fixtures_dir().join(format!("{name}.tar"))
}

/// The line `git-fixtures/GIT_VERSION` holds: what `git --version` prints for the git that
/// makes the archives.
fn declared_version() -> String {
    let path = fixtures_dir().join("GIT_VERSION");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    text.trim_end().to_owned()
}

/// The text of fixture `name`'s script, `git-fixtures/scripts/<name>.sh`.
fn script_text(name: &str) -> String {
    let path = scripts_dir().join(format!("{name}.sh"));
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// Why the git whose `git --version` printed `version` is too old for the fixture script
/// `script`: a `# needs git X.Y` line among the comment lines the script starts with names a
/// later release. `None` when they name none, or one no later than `version`'s.
fn too_old(version: &str, script: &str) -> Option<String> {
    let needed = script
        .lines()
        .take_while(|line| line.starts_with('#'))
        .find_map(|line| line.strip_prefix("# needs git "))?
        .trim_end();
    let needs = release(needed)
        .unwrap_or_else(|| panic!("`# needs git {needed}` names no git release X.Y"));
    let has = version
        .strip_prefix("git version ")
        .and_then(release)
        .unwrap_or_else(|| panic!("git --version printed {version:?}, which names no release"));
    (has < needs).then(|| format!("it needs git {needed}, and {version} is older"))
}

/// The release of git `text` starts with, its numbers X.Y.Z: `2.45` (Z is 0), `2.43.0`, or the
/// start of `2.45.0.rc1` or `2.47.1 (Apple Git-154)`. `None` when it starts with no X.Y.
fn release(text: &str) -> Option<[u32; 3]> {
    let end = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(text.len());
    let mut parts = text[..end].split('.');
    let mut numbers = [0; 3];
    for (at, number) in numbers.iter_mut().enumerate() {
        match parts.next().filter(|part| !part.is_empty()) {
            Some(part) => *number = part.parse().ok()?,
            None if at == 2 => break,
            None => return None,
        }
    }
    Some(numbers)
}

/// What `git --version` printed for the git that made the fixtures [`fixture`] takes, and which
/// git that is: the declared one (`git-fixtures/GIT_VERSION`) for the archives; with [`DIR_ENV`]
/// set, the one [`check`] ran, which wrote its line to `<dir>/GIT_VERSION`.
fn fixtures_git() -> (String, String) {
    let Some(dir) = std::env::var_os(DIR_ENV) else {
        let whose = "the declared git of git-fixtures/GIT_VERSION, which made the archives";
        return (declared_version(), whose.to_owned());
    };
    let path = Path::new(&dir).join("GIT_VERSION");
    let text = fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "read {}: {error}: {DIR_ENV} names a directory of fixtures that check_git_fixtures \
             built, which names the git it ran there",
            path.display(),
        )
    });
    let whose = format!(
        "the git that built the fixtures in {}",
        Path::new(&dir).display()
    );
    (text.trim_end().to_owned(), whose)
}

/// Why fixture `name` has no archive, naming the script that builds it, if there is one.
fn missing(name: &str) -> String {
    let script = scripts_dir().join(format!("{name}.sh"));
    if script.exists() {
        format!(
            "the git fixture {name} has no archive: `just git-fixtures` builds git-fixtures/{name}.tar from {}",
            script.display(),
        )
    } else {
        format!(
            "there is no git fixture {name}: neither git-fixtures/{name}.tar nor a script {}",
            script.display(),
        )
    }
}

/// Fixture `name`'s archive, extracted into a fresh temporary directory.
fn extract_fixture(name: &str) -> GitFixture {
    let archive = archive_path(name);
    let file = match File::open(&archive) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => panic!("{}", missing(name)),
        Err(error) => panic!("open {}: {error}", archive.display()),
    };
    let temp = tempfile::tempdir().expect("create a temp dir");
    let dir = temp.path().join(name);
    let mut tar = tar::Archive::new(file);
    tar.set_preserve_mtime(false);
    tar.unpack(&dir)
        .unwrap_or_else(|error| panic!("extract {}: {error}", archive.display()));
    GitFixture::at(temp, dir)
}

/// A copy of the fixture `src`, which a [`check`] built, in a fresh temporary directory, without
/// what an archive leaves out.
fn copy_fixture(name: &str, src: &Path) -> GitFixture {
    assert!(
        src.is_dir(),
        "{DIR_ENV} holds no git fixture {name}: {} is no directory",
        src.display(),
    );
    let temp = tempfile::tempdir().expect("create a temp dir");
    let dir = temp.path().join(name);
    fs::create_dir(&dir).expect("create a fixture's directory");
    for entry in entries(src) {
        let to = dir.join(&entry.path);
        match entry.kind {
            EntryKind::Dir => fs::create_dir(&to).expect("create a fixture's directory"),
            EntryKind::File { .. } => {
                fs::copy(src.join(&entry.path), &to).expect("copy a fixture's file");
            }
        }
    }
    GitFixture::at(temp, dir)
}

impl GitFixture {
    /// The fixture at `dir`, in `temp`: its repository and manifest, if it has them.
    fn at(temp: TempDir, dir: PathBuf) -> GitFixture {
        let repo = dir.join("repo.git");
        let repo = repo.is_dir().then_some(repo);
        let manifest = dir.join("manifest.json");
        let manifest = manifest.exists().then(|| read_manifest(&manifest));
        GitFixture {
            dir,
            repo,
            manifest,
            _temp: temp,
        }
    }
}

/// The manifest at `path`.
fn read_manifest(path: &Path) -> Manifest {
    let text =
        fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

/// What a fixture's directory holds that its archive keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryKind {
    /// A directory.
    Dir,
    /// A regular file, executable or not.
    File {
        /// Whether any execute bit is set.
        exec: bool,
    },
}

/// One entry of a fixture's directory.
#[derive(Debug)]
struct Entry {
    /// Its path, relative to the fixture's directory.
    path: PathBuf,
    /// What it is.
    kind: EntryKind,
}

/// Everything under `root` but what [`LEFT_OUT`] names, sorted by path, each directory before
/// what it holds. Anything but a directory or a regular file (a symlink, say) fails the test.
fn entries(root: &Path) -> Vec<Entry> {
    let mut entries = Vec::new();
    walk(root, Path::new(""), &mut entries);
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries
}

/// Appends to `entries` everything under `root/rel`, but what [`LEFT_OUT`] names.
fn walk(root: &Path, rel: &Path, entries: &mut Vec<Entry>) {
    let dir = root.join(rel);
    let listing =
        fs::read_dir(&dir).unwrap_or_else(|error| panic!("read {}: {error}", dir.display()));
    for item in listing {
        let path = rel.join(item.expect("read a directory entry").file_name());
        if left_out(&path) {
            continue;
        }
        let full = root.join(&path);
        let metadata = fs::symlink_metadata(&full)
            .unwrap_or_else(|error| panic!("stat {}: {error}", full.display()));
        if metadata.is_dir() {
            entries.push(Entry {
                path: path.clone(),
                kind: EntryKind::Dir,
            });
            walk(root, &path, entries);
        } else if metadata.is_file() {
            let exec = metadata.permissions().mode() & 0o111 != 0;
            entries.push(Entry {
                path,
                kind: EntryKind::File { exec },
            });
        } else {
            panic!(
                "a git fixture holds only directories and regular files: {} is neither",
                full.display(),
            );
        }
    }
}

/// Whether `path`, relative to a fixture's directory, is one an archive leaves out.
fn left_out(path: &Path) -> bool {
    let mut components = path.components();
    components.next() == Some(Component::Normal("repo.git".as_ref()))
        && components
            .next()
            .is_some_and(|component| LEFT_OUT.iter().any(|name| component.as_os_str() == *name))
}

/// Writes what `dir` holds to the archive `path`, deterministically: entries sorted by path,
/// mtime 0, uid and gid 0, no user or group name, mode 0755 for a directory or an executable
/// file and 0644 for any other file. The archive replaces `path` whole.
fn write_archive(dir: &Path, path: &Path) {
    let mut builder = tar::Builder::new(Vec::new());
    for entry in entries(dir) {
        let mut header = tar::Header::new_gnu();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        let appended = match entry.kind {
            EntryKind::Dir => {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_mode(0o755);
                header.set_size(0);
                builder.append_data(&mut header, &entry.path, io::empty())
            }
            EntryKind::File { exec } => {
                let bytes = fs::read(dir.join(&entry.path)).expect("read a fixture's file");
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(if exec { 0o755 } else { 0o644 });
                header.set_size(bytes.len().try_into().expect("a file's length fits in u64"));
                builder.append_data(&mut header, &entry.path, bytes.as_slice())
            }
        };
        appended.unwrap_or_else(|error| panic!("archive {}: {error}", entry.path.display()));
    }
    let bytes = builder.into_inner().expect("finish an archive");
    let temp = path.with_extension("tar.tmp");
    fs::write(&temp, bytes).unwrap_or_else(|error| panic!("write {}: {error}", temp.display()));
    fs::rename(&temp, path).unwrap_or_else(|error| panic!("rename to {}: {error}", path.display()));
}

/// The git the harness runs, in the pinned environment.
struct Git {
    /// The binary: [`GIT_ENV`]'s, or `git`.
    program: PathBuf,
    /// The scripts' and git's `PATH`: the caller's, with [`GIT_ENV`]'s directory first.
    path: OsString,
    /// An empty directory, their `HOME`.
    home: TempDir,
}

impl Git {
    /// The git [`GIT_ENV`] names, or `git` on `PATH`.
    fn from_env() -> Git {
        let inherited = std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin"));
        let (program, path) = match std::env::var_os(GIT_ENV) {
            None => (PathBuf::from("git"), inherited),
            Some(program) => {
                let program = PathBuf::from(program);
                assert!(
                    program.is_absolute() && program.file_name().is_some_and(|name| name == "git"),
                    "{GIT_ENV} is the absolute path of a file named git, whose directory goes \
                     first on the scripts' PATH: {}",
                    program.display(),
                );
                let first = program.parent().expect("an absolute path has a parent");
                let dirs =
                    std::iter::once(first.to_path_buf()).chain(std::env::split_paths(&inherited));
                let path = std::env::join_paths(dirs).expect("join PATH");
                (program, path)
            }
        };
        Git {
            program,
            path,
            home: tempfile::tempdir().expect("create a temp dir"),
        }
    }

    /// `command`, in the pinned environment.
    fn pin<'c>(&self, command: &'c mut Command) -> &'c mut Command {
        command
            .env_clear()
            .env("PATH", &self.path)
            .env("HOME", self.home.path())
            .envs(PINNED);
        if let Some(tmpdir) = std::env::var_os("TMPDIR") {
            command.env("TMPDIR", tmpdir);
        }
        command
    }

    /// What `git --version` prints, without its line feed.
    fn version(&self) -> String {
        let output = child::run(self.pin(Command::new(&self.program).arg("--version")));
        assert!(
            output.status.success(),
            "git --version: {}",
            failure(&output)
        );
        text(&output.stdout)
    }

    /// Runs git with `args` on the repository `repo`, replacement objects off, to its end.
    fn output(&self, repo: &Path, args: &[&str]) -> Output {
        let mut git_dir = OsString::from("--git-dir=");
        git_dir.push(repo);
        let mut command = Command::new(&self.program);
        self.pin(&mut command)
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .arg(git_dir)
            .args(args);
        child::run(&mut command)
    }

    /// What git with `args` on the repository `repo` prints; one that fails fails the test.
    fn stdout(&self, repo: &Path, args: &[&str]) -> Vec<u8> {
        let output = self.output(repo, args);
        assert!(
            output.status.success(),
            "git {}: {}",
            args.join(" "),
            failure(&output),
        );
        output.stdout
    }
}

/// How a run that failed ended, and what it printed on stderr.
fn failure(output: &Output) -> String {
    format!(
        "{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim_end(),
    )
}

/// `bytes`, which git printed as text, without the line feeds that end it.
fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec())
        .expect("git prints UTF-8 here")
        .trim_end_matches('\n')
        .to_owned()
}

/// Builds fixture `name` into `out`, a directory that does not exist yet: its script, run in
/// the pinned environment, then the manifest of the repository it left, if any. The error is
/// the script's failure, with its stderr.
fn build(git: &Git, name: &str, out: &Path) -> Result<(), String> {
    let script = scripts_dir().join(format!("{name}.sh"));
    fs::create_dir_all(out).unwrap_or_else(|error| panic!("create {}: {error}", out.display()));
    let output = child::run(git.pin(Command::new("sh").arg(&script).arg(out)));
    if !output.status.success() {
        return Err(format!("{} failed: {}", script.display(), failure(&output)));
    }
    let repo = out.join("repo.git");
    if repo.is_dir() {
        let manifest = manifest_of(git, &repo);
        let json = serde_json::to_string_pretty(&manifest).expect("serialize a manifest") + "\n";
        fs::write(out.join("manifest.json"), json).expect("write a manifest");
    }
    Ok(())
}

/// What git's plumbing reads from the repository `repo`.
fn manifest_of(git: &Git, repo: &Path) -> Manifest {
    let head = {
        let output = git.output(repo, &["symbolic-ref", "-q", "HEAD"]);
        match output.status.code() {
            Some(0) => Some(text(&output.stdout)),
            Some(1) => None,
            _ => panic!("git symbolic-ref -q HEAD: {}", failure(&output)),
        }
    };
    let shallow = match text(&git.stdout(repo, &["rev-parse", "--is-shallow-repository"])).as_str()
    {
        "true" => true,
        "false" => false,
        other => panic!("git rev-parse --is-shallow-repository printed {other:?}"),
    };
    let objects = Objects::read(git, repo);
    let commits = text(&git.stdout(
        repo,
        &[
            "rev-list",
            "--topo-order",
            "--reverse",
            "--parents",
            "--all",
        ],
    ))
    .lines()
    .map(|line| objects.commit(line))
    .collect();
    Manifest {
        blobs: objects.blobs,
        commits,
        git_version: git.version(),
        head,
        object_format: text(&git.stdout(repo, &["rev-parse", "--show-object-format"])),
        refs: refs_of(git, repo),
        shallow,
        trees: objects.trees,
    }
}

/// Every ref of the repository `repo` that peels to a commit, and that commit.
fn refs_of(git: &Git, repo: &Path) -> BTreeMap<String, String> {
    let listing = text(&git.stdout(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname) %(objecttype) %(objectname) %(*objecttype) %(*objectname)",
        ],
    ));
    let mut refs = BTreeMap::new();
    for line in listing.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let [name, kind, oid, peeled_kind, peeled] = fields[..] else {
            panic!("git for-each-ref printed {line:?}");
        };
        let commit = match (kind, peeled_kind) {
            ("commit", _) => Some(oid.to_owned()),
            ("tag", "commit") => Some(peeled.to_owned()),
            // A tag of a tag: peeled all the way, if it ends at a commit.
            ("tag", _) => {
                let output = git.output(
                    repo,
                    &["rev-parse", "--verify", "-q", &format!("{oid}^{{commit}}")],
                );
                output.status.success().then(|| text(&output.stdout))
            }
            _ => None,
        };
        if let Some(commit) = commit {
            refs.insert(name.to_owned(), commit);
        }
    }
    refs
}

/// The objects a repository holds, as `cat-file --batch-all-objects --batch` lists them: it
/// reads only what is there (a partial clone fetches nothing), and needs nothing on stdin.
#[derive(Default)]
struct Objects {
    /// Every commit's bytes, by id.
    commits: BTreeMap<String, Vec<u8>>,
    /// Every tree's bytes, by id.
    trees: BTreeMap<String, Vec<u8>>,
    /// Every blob's size, by id.
    blobs: BTreeMap<String, u64>,
}

impl Objects {
    /// The objects of the repository `repo`.
    fn read(git: &Git, repo: &Path) -> Objects {
        let stream = git.stdout(repo, &["cat-file", "--batch-all-objects", "--batch"]);
        let mut objects = Objects::default();
        let mut rest = stream.as_slice();
        while !rest.is_empty() {
            let end = rest
                .iter()
                .position(|&byte| byte == b'\n')
                .expect("cat-file --batch ends its header lines");
            let header =
                std::str::from_utf8(&rest[..end]).expect("cat-file --batch prints ASCII headers");
            let fields: Vec<&str> = header.split(' ').collect();
            let [oid, kind, size] = fields[..] else {
                panic!("git cat-file --batch printed the header {header:?}");
            };
            let size: u64 = size.parse().expect("an object's size");
            let len = usize::try_from(size).expect("an object's size fits in usize");
            let body = &rest[end + 1..end + 1 + len];
            assert_eq!(
                rest.get(end + 1 + len),
                Some(&b'\n'),
                "cat-file --batch ends {oid}"
            );
            match kind {
                "commit" => {
                    objects.commits.insert(oid.to_owned(), body.to_vec());
                }
                "tree" => {
                    objects.trees.insert(oid.to_owned(), body.to_vec());
                }
                "blob" => {
                    objects.blobs.insert(oid.to_owned(), size);
                }
                "tag" => {}
                other => panic!("git cat-file --batch printed {oid} of the type {other}"),
            }
            rest = &rest[end + 2 + len..];
        }
        objects
    }

    /// The manifest's commit for a line of `rev-list --parents`: its id, then its parents'.
    fn commit(&self, line: &str) -> ManifestCommit {
        let mut ids = line.split(' ');
        let oid = ids.next().expect("rev-list prints a commit a line");
        let raw = self.commits.get(oid).unwrap_or_else(|| {
            panic!(
                "git rev-list names the commit {oid}, which cat-file --batch-all-objects does not"
            )
        });
        let tree = raw
            .strip_prefix(b"tree ")
            .and_then(|rest| rest.split(|&byte| byte == b'\n').next())
            .and_then(|tree| std::str::from_utf8(tree).ok())
            .unwrap_or_else(|| panic!("the commit {oid} does not start with its tree"));
        ManifestCommit {
            oid: oid.to_owned(),
            parents: ids.map(str::to_owned).collect(),
            raw: raw.clone(),
            tree: tree.to_owned(),
        }
    }
}

/// Appends to `differences` a line for each field in which the manifests of fixture `name`, the
/// archive's and the one rebuilt, differ, `git_version` aside.
fn compare_manifests(
    differences: &mut Vec<String>,
    name: &str,
    archived: Option<&Manifest>,
    rebuilt: Option<&Manifest>,
) {
    let (archived, rebuilt) = match (archived, rebuilt) {
        (None, None) => return,
        (Some(_), None) => {
            differences.push(format!(
                "{name}: the archive has a manifest, the rebuilt fixture none"
            ));
            return;
        }
        (None, Some(_)) => {
            differences.push(format!(
                "{name}: the rebuilt fixture has a manifest, the archive none"
            ));
            return;
        }
        (Some(archived), Some(rebuilt)) => (archived, rebuilt),
    };
    let mut scalar = |field: &str, a: String, b: String| {
        if a != b {
            differences.push(format!(
                "{name}: {field}: the archive's is {a}, the rebuilt one's {b}"
            ));
        }
    };
    scalar(
        "object_format",
        archived.object_format.clone(),
        rebuilt.object_format.clone(),
    );
    scalar(
        "head",
        format!("{:?}", archived.head),
        format!("{:?}", rebuilt.head),
    );
    scalar(
        "shallow",
        archived.shallow.to_string(),
        rebuilt.shallow.to_string(),
    );
    compare_maps(
        differences,
        name,
        "ref",
        &archived.refs,
        &rebuilt.refs,
        Clone::clone,
    );
    compare_commits(differences, name, &archived.commits, &rebuilt.commits);
    compare_maps(
        differences,
        name,
        "tree",
        &archived.trees,
        &rebuilt.trees,
        |bytes| STANDARD.encode(bytes),
    );
    compare_maps(
        differences,
        name,
        "blob",
        &archived.blobs,
        &rebuilt.blobs,
        |size| format!("{size} bytes"),
    );
}

/// Appends to `differences` a line for each key of `field` only one side has, or the sides give
/// different values (each shown by `show`).
fn compare_maps<V: PartialEq>(
    differences: &mut Vec<String>,
    name: &str,
    field: &str,
    archived: &BTreeMap<String, V>,
    rebuilt: &BTreeMap<String, V>,
    show: impl Fn(&V) -> String,
) {
    for (key, value) in archived {
        match rebuilt.get(key) {
            None => differences.push(format!("{name}: {field} {key}: only in the archive")),
            Some(other) if other != value => differences.push(format!(
                "{name}: {field} {key}: the archive's is {}, the rebuilt one's {}",
                show(value),
                show(other),
            )),
            Some(_) => {}
        }
    }
    for key in rebuilt.keys().filter(|key| !archived.contains_key(*key)) {
        differences.push(format!(
            "{name}: {field} {key}: only in the rebuilt fixture"
        ));
    }
}

/// Appends to `differences` a line for each commit only one side has, each field of a commit
/// both have that differs, and, when both have the same commits, the first place where their
/// orders part.
fn compare_commits(
    differences: &mut Vec<String>,
    name: &str,
    archived: &[ManifestCommit],
    rebuilt: &[ManifestCommit],
) {
    let by_oid = |commits: &[ManifestCommit]| -> BTreeMap<String, ManifestCommit> {
        commits
            .iter()
            .map(|commit| (commit.oid.clone(), commit.clone()))
            .collect()
    };
    let (archived_by_oid, rebuilt_by_oid) = (by_oid(archived), by_oid(rebuilt));
    for (oid, commit) in &archived_by_oid {
        let Some(other) = rebuilt_by_oid.get(oid) else {
            differences.push(format!("{name}: commit {oid}: only in the archive"));
            continue;
        };
        if commit.parents != other.parents {
            differences.push(format!(
                "{name}: commit {oid}: the archive's parents are {:?}, the rebuilt one's {:?}",
                commit.parents, other.parents,
            ));
        }
        if commit.tree != other.tree {
            differences.push(format!(
                "{name}: commit {oid}: the archive's tree is {}, the rebuilt one's {}",
                commit.tree, other.tree,
            ));
        }
        if commit.raw != other.raw {
            differences.push(format!("{name}: commit {oid}: its bytes differ"));
        }
    }
    for oid in rebuilt_by_oid
        .keys()
        .filter(|oid| !archived_by_oid.contains_key(*oid))
    {
        differences.push(format!("{name}: commit {oid}: only in the rebuilt fixture"));
    }
    let same_commits = archived_by_oid.keys().eq(rebuilt_by_oid.keys());
    if let Some(at) = archived
        .iter()
        .zip(rebuilt)
        .position(|(a, b)| a.oid != b.oid)
        .filter(|_| same_commits)
    {
        differences.push(format!(
            "{name}: commits: the same commits in another order, from #{at} on: the archive's \
             {}, the rebuilt one's {}",
            archived[at].oid, rebuilt[at].oid,
        ));
    }
}

/// An empty bare repository, as `git init --bare` makes one, in a temporary directory of its own
/// that lives as long as it does: the destination of an export, made without running git.
#[derive(Debug)]
pub struct EmptyRepository {
    /// The repository's directory, `repo.git`.
    pub dir: PathBuf,
    /// The temporary directory that holds `dir`.
    _temp: TempDir,
}

/// An empty bare repository: the files `git init --bare` writes, written by hand so that no test
/// runs git. `HEAD` names `refs/heads/main`, which does not exist yet; `config` holds git's
/// `[core]` section for a bare repository (`repositoryformatversion = 0`, `filemode = true`,
/// `bare = true`); `objects/info`, `objects/pack`, `refs/heads` and `refs/tags` are empty
/// directories. git's sample hooks, `description` and `info/exclude`, which no command needs, are
/// left out.
pub fn empty_repository() -> EmptyRepository {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let dir = temp.path().join("repo.git");
    for sub in ["objects/info", "objects/pack", "refs/heads", "refs/tags"] {
        let path = dir.join(sub);
        fs::create_dir_all(&path)
            .unwrap_or_else(|error| panic!("create {}: {error}", path.display()));
    }
    let files = [
        ("HEAD", "ref: refs/heads/main\n"),
        (
            "config",
            "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = true\n",
        ),
    ];
    for (name, text) in files {
        let path = dir.join(name);
        fs::write(&path, text).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
    }
    EmptyRepository { dir, _temp: temp }
}

/// Writes `bytes` into the git directory `repo` (an [`empty_repository`]'s, say) as the loose
/// object file of `oid` (40 lowercase hex digits), `objects/<2 hex>/<38 hex>`, wrapped in zlib as
/// git keeps a loose object but left uncompressed (stored deflate blocks): a file made without
/// running git. `bytes` are the object as git hashes it, its `<kind> <len>\0` header and its
/// content, or what a crafted file holds instead, such as a header that claims another length than
/// what follows; nothing checks that `oid` is their hash. The path of the file written.
pub fn write_loose_object(repo: &Path, oid: &str, bytes: &[u8]) -> PathBuf {
    assert!(
        oid.len() == 40 && oid.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "{oid} is no SHA-1 object id"
    );
    // zlib (RFC 1950): a header for deflate with a 32 KiB window and no dictionary, then deflate's
    // stored blocks (RFC 1951 §3.2.4), each its final flag, its length and that length's
    // complement, then its bytes, and last the Adler-32 of `bytes`, most significant byte first.
    let mut file = vec![0x78, 0x01];
    let mut blocks = bytes.chunks(usize::from(u16::MAX)).peekable();
    if blocks.peek().is_none() {
        file.extend_from_slice(&[0x01, 0x00, 0x00, 0xff, 0xff]);
    }
    while let Some(block) = blocks.next() {
        let len = u16::try_from(block.len()).expect("a block holds at most 65,535 bytes");
        file.push(u8::from(blocks.peek().is_none()));
        file.extend_from_slice(&len.to_le_bytes());
        file.extend_from_slice(&(!len).to_le_bytes());
        file.extend_from_slice(block);
    }
    let (mut a, mut b) = (1_u32, 0_u32);
    for &byte in bytes {
        a = (a + u32::from(byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    file.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let dir = repo.join("objects").join(&oid[..2]);
    fs::create_dir_all(&dir).unwrap_or_else(|error| panic!("create {}: {error}", dir.display()));
    let path = dir.join(&oid[2..]);
    fs::write(&path, file).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
    path
}

/// `Vec<u8>` as standard padded base64.
mod base64_bytes {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    /// The base64 of `bytes`.
    pub(super) fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    /// The bytes of a base64 string.
    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        STANDARD.decode(text).map_err(serde::de::Error::custom)
    }
}

/// A map's `Vec<u8>` values as standard padded base64.
mod base64_values {
    use std::collections::BTreeMap;

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    /// The map, its values in base64.
    pub(super) fn serialize<S: Serializer>(
        map: &BTreeMap<String, Vec<u8>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        map.iter()
            .map(|(key, bytes)| (key, STANDARD.encode(bytes)))
            .collect::<BTreeMap<_, _>>()
            .serialize(serializer)
    }

    /// The map, its base64 values decoded.
    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<String, Vec<u8>>, D::Error> {
        BTreeMap::<String, String>::deserialize(deserializer)?
            .into_iter()
            .map(|(key, text)| {
                Ok((
                    key,
                    STANDARD.decode(text).map_err(serde::de::Error::custom)?,
                ))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fmt::Write as _;

    use super::*;

    #[test]
    fn the_fixture_scripts_have_archives_and_manifests() {
        let scripts = scripts();
        assert!(
            !scripts.is_empty(),
            "no fixture script in {}",
            scripts_dir().display()
        );
        let (made_by, _) = fixtures_git();
        for name in &scripts {
            if let Some(why) = skipped(name) {
                eprintln!("{name}: skipped: {why}");
                continue;
            }
            let fixture = fixture(name);
            if fixture.repo.is_none() {
                continue;
            }
            let manifest = fixture
                .manifest
                .as_ref()
                .unwrap_or_else(|| panic!("{name}: a repository without a manifest"));
            assert_eq!(manifest.git_version, made_by, "{name}: made by another git");
            assert!(!manifest.commits.is_empty(), "{name}: no commit");
            let mut earlier = BTreeSet::new();
            for commit in &manifest.commits {
                for parent in &commit.parents {
                    assert!(
                        earlier.contains(parent.as_str()),
                        "{name}: the commit {} comes before its parent {parent}",
                        commit.oid,
                    );
                }
                assert!(
                    manifest.trees.contains_key(&commit.tree),
                    "{name}: the commit {}'s tree {} is not in the manifest",
                    commit.oid,
                    commit.tree,
                );
                earlier.insert(commit.oid.as_str());
            }
            for (name_of_ref, oid) in &manifest.refs {
                assert!(
                    earlier.contains(oid.as_str()),
                    "{name}: {name_of_ref} names {oid}, which is not a commit of the manifest",
                );
            }
        }
        // Every archive is a script's, and one the declared git makes.
        let declared = declared_version();
        for item in fs::read_dir(fixtures_dir()).expect("read git-fixtures") {
            let path = item.expect("read a directory entry").path();
            if path.extension().is_some_and(|extension| extension == "tar") {
                let stem = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .expect("a UTF-8 name");
                assert!(
                    scripts.iter().any(|name| name == stem),
                    "{} has no script: scripts/{stem}.sh",
                    path.display(),
                );
                assert_eq!(
                    too_old(&declared, &script_text(stem)),
                    None,
                    "{} is an archive the declared git does not make",
                    path.display(),
                );
            }
        }
    }

    #[test]
    fn the_linear_fixture_is_five_commits_on_main() {
        let fixture = fixture("linear");
        assert!(fixture.repo.is_some(), "linear has a repository");
        let manifest = fixture.manifest.as_ref().expect("linear has a manifest");
        let oids: Vec<&str> = manifest
            .commits
            .iter()
            .map(|commit| commit.oid.as_str())
            .collect();
        assert_eq!(oids.len(), 5, "{oids:?}");
        for (at, commit) in manifest.commits.iter().enumerate() {
            let parents: Vec<&str> = commit.parents.iter().map(String::as_str).collect();
            assert_eq!(
                parents,
                oids[..at].last().into_iter().copied().collect::<Vec<_>>()
            );
        }
        assert_eq!(
            manifest.refs.get("refs/heads/main").map(String::as_str),
            Some(oids[4])
        );
        assert_eq!(manifest.head.as_deref(), Some("refs/heads/main"));
    }

    /// What a crafted fixture's script says it made: the fixture, the claim, and its check on
    /// the fixture's manifest.
    type Claim = (&'static str, &'static str, fn(&Manifest) -> bool);

    /// The empty tree's id in a SHA-1 repository.
    const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

    /// The claims of the fixture set's scripts that the git import's tests build on, as the
    /// manifests show them. What the import makes of each fixture is for the tests of
    /// `swarmops-git`.
    const CLAIMS: [Claim; 19] = [
        (
            "roots",
            "three roots, one of them the empty tree's",
            |manifest| {
                let roots = || {
                    manifest
                        .commits
                        .iter()
                        .filter(|commit| commit.parents.is_empty())
                };
                roots().count() >= 3 && roots().any(|commit| commit.tree == EMPTY_TREE)
            },
        ),
        (
            "roots",
            "a message without a final line feed, and an empty one",
            |manifest| {
                let raws = || manifest.commits.iter().map(|commit| commit.raw.as_slice());
                raws().any(|raw| !raw.ends_with(b"\n")) && raws().any(|raw| raw.ends_with(b"\n\n"))
            },
        ),
        (
            "roots",
            "an empty commit, of its parent's tree",
            |manifest| {
                manifest
                    .commits
                    .iter()
                    .any(|commit| match &commit.parents[..] {
                        [parent] => commit_of(manifest, parent).tree == commit.tree,
                        _ => false,
                    })
            },
        ),
        (
            "signatures",
            "author times -86400 and -0000, offsets +0530, -1200 and +1400",
            |manifest| {
                [" -86400 +0000", " -0000", " +0530", " -1200", " +1400"]
                    .iter()
                    .all(|end| {
                        header_lines(manifest).any(|line| {
                            line.starts_with(b"author ") && line.ends_with(end.as_bytes())
                        })
                    })
            },
        ),
        (
            "signatures",
            "an author name that is not UTF-8",
            |manifest| {
                header_lines(manifest)
                    .any(|line| line.starts_with(b"author ") && std::str::from_utf8(line).is_err())
            },
        ),
        (
            "headers",
            "encoding, gpgsig with a blank continuation line, mergetag, x-custom",
            |manifest| {
                [
                    "\nencoding ISO-8859-1\n",
                    "\ngpgsig ",
                    "\n \n",
                    "\nmergetag object ",
                    "\nx-custom ",
                ]
                .iter()
                .all(|part| {
                    manifest
                        .commits
                        .iter()
                        .any(|commit| contains(&commit.raw, part.as_bytes()))
                })
            },
        ),
        (
            "many_parents",
            "main a merge of 255 roots, too-many a merge of 256",
            |manifest| {
                // How many roots the branch's tip merges: its parents, if they are all roots and
                // no two the same; otherwise none.
                let roots_merged = |branch: &str| {
                    let parents = &commit_of(manifest, &manifest.refs[branch]).parents;
                    let distinct: BTreeSet<&String> = parents.iter().collect();
                    let roots = parents
                        .iter()
                        .all(|parent| commit_of(manifest, parent).parents.is_empty());
                    if roots && distinct.len() == parents.len() {
                        parents.len()
                    } else {
                        0
                    }
                };
                roots_merged("refs/heads/main") == 255 && roots_merged("refs/heads/too-many") == 256
            },
        ),
        (
            "gitlinks",
            "a gitlink sub beside another entry, and a tree that holds nothing but a gitlink",
            |manifest| {
                let trees: Vec<Vec<StoredEntry>> = manifest
                    .trees
                    .values()
                    .map(|bytes| stored_entries(bytes))
                    .collect();
                trees.iter().any(|entries| {
                    entries.len() > 1
                        && entries
                            .iter()
                            .any(|entry| entry.mode == b"160000" && entry.name == b"sub")
                }) && trees
                    .iter()
                    .any(|entries| matches!(&entries[..], [only] if only.mode == b"160000"))
            },
        ),
        (
            "paths",
            "a name that is not UTF-8, and the name ..",
            |manifest| {
                let entries = every_entry(manifest);
                entries
                    .iter()
                    .any(|entry| std::str::from_utf8(&entry.name).is_err())
                    && entries.iter().any(|entry| entry.name == b"..")
            },
        ),
        (
            "modes_odd",
            "entries of the modes 100664, 100600 and 040000",
            |manifest| {
                let entries = every_entry(manifest);
                [&b"100664"[..], b"100600", b"040000"]
                    .iter()
                    .all(|mode| entries.iter().any(|entry| entry.mode == *mode))
            },
        ),
        (
            "tree_order",
            "trees out of git's order, in byte order or not, and a name twice",
            |manifest| {
                let trees: Vec<Vec<StoredEntry>> = manifest
                    .trees
                    .values()
                    .map(|bytes| stored_entries(bytes))
                    .collect();
                let any_pair = |test: fn(&StoredEntry, &StoredEntry) -> bool| {
                    trees
                        .iter()
                        .any(|entries| entries.windows(2).any(|pair| test(&pair[0], &pair[1])))
                };
                any_pair(|a, b| a.name > b.name)
                    && any_pair(|a, b| a.name < b.name && order_key(a) > order_key(b))
                    && any_pair(|a, b| a.name == b.name)
            },
        ),
        (
            "empty_trees",
            "a directory entry named empty that names the empty tree",
            |manifest| {
                every_entry(manifest).iter().any(|entry| {
                    entry.name == b"empty" && is_dir(&entry.mode) && entry.id == EMPTY_TREE
                })
            },
        ),
        (
            "nonstandard",
            "two spaces before <, the offset +05:30, no blank line, a parent twice",
            |manifest| {
                let raws = || manifest.commits.iter().map(|commit| commit.raw.as_slice());
                raws().any(|raw| contains(raw, b"  <"))
                    && raws().any(|raw| contains(raw, b" +05:30\n"))
                    && raws().any(|raw| !contains(raw, b"\n\n"))
                    && manifest
                        .commits
                        .iter()
                        .any(|commit| commit.parents.windows(2).any(|pair| pair[0] == pair[1]))
            },
        ),
        ("nonstandard", "a header line without a value", |manifest| {
            header_lines(manifest).any(|line| !line.starts_with(b" ") && !line.contains(&b' '))
        }),
        ("sha256", "the object format sha256", |manifest| {
            manifest.object_format == "sha256"
        }),
        ("shallow", "a shallow clone", |manifest| manifest.shallow),
        (
            "partial_clone",
            "a tree that names a blob the repository lacks",
            |manifest| {
                every_entry(manifest).iter().any(|entry| {
                    !is_dir(&entry.mode)
                        && entry.mode != b"160000"
                        && !manifest.blobs.contains_key(&entry.id)
                })
            },
        ),
        (
            "refs",
            "newer three commits past older, merged's second parent older, rewritten without it",
            |manifest| {
                let tip = |name: &str| manifest.refs[&format!("refs/heads/{name}")].clone();
                let older = tip("older");
                let newer = reaches(manifest, &tip("newer"));
                let merged = commit_of(manifest, &tip("merged"));
                newer.contains(&older)
                    && newer.difference(&reaches(manifest, &older)).count() == 3
                    && merged.parents.get(1) == Some(&older)
                    && !reaches(manifest, &merged.parents[0]).contains(&older)
                    && !reaches(manifest, &tip("rewritten")).contains(&older)
            },
        ),
        (
            "replace_ref",
            "a replacement ref that names another commit",
            |manifest| {
                manifest.refs.iter().any(|(name, oid)| {
                    name.strip_prefix("refs/replace/")
                        .is_some_and(|replaced| replaced != oid)
                })
            },
        ),
    ];

    /// What a fixture's script says its repository holds on disk: the fixture, the claim, and
    /// its check on the fixture's `repo.git`.
    type Layout = (&'static str, &'static str, fn(&Path) -> bool);

    /// The claims of two scripts of the fixture set about their repositories' files.
    const LAYOUTS: [Layout; 2] = [
        (
            "loose_and_packed",
            "loose and packed objects, loose and packed refs",
            |repo| {
                let files = |dir: &str| {
                    entries(&repo.join(dir))
                        .into_iter()
                        .filter(|entry| entry.kind != EntryKind::Dir)
                        .map(|entry| entry.path)
                        .collect::<Vec<_>>()
                };
                files("objects/pack").iter().any(|path| {
                    path.extension()
                        .is_some_and(|extension| extension == "pack")
                }) && !loose_objects(repo).is_empty()
                    && repo.join("packed-refs").is_file()
                    && !files("refs").is_empty()
            },
        ),
        (
            "corrupt_object",
            "a loose object whose file is a copy of another's",
            |repo| {
                let files: Vec<Vec<u8>> = loose_objects(repo)
                    .iter()
                    .map(|path| fs::read(path).expect("read a loose object"))
                    .collect();
                files
                    .iter()
                    .enumerate()
                    .any(|(at, bytes)| files[at + 1..].contains(bytes))
            },
        ),
    ];

    #[test]
    fn the_crafted_fixtures_hold_what_their_scripts_say() {
        for (name, claim, holds) in CLAIMS {
            let fixture = fixture(name);
            let manifest = fixture
                .manifest
                .as_ref()
                .unwrap_or_else(|| panic!("{name}: no manifest"));
            assert!(holds(manifest), "{name}: not {claim}");
        }
        for (name, claim, holds) in LAYOUTS {
            let fixture = fixture(name);
            let repo = fixture
                .repo
                .as_ref()
                .unwrap_or_else(|| panic!("{name}: no repository"));
            assert!(holds(repo), "{name}: not {claim}");
        }
    }

    /// An entry of a tree, as git stores it.
    struct StoredEntry {
        /// Its mode's octal digits, as stored.
        mode: Vec<u8>,
        /// Its name.
        name: Vec<u8>,
        /// The id of what it names, in hex.
        id: String,
    }

    /// The entries of the tree `bytes` of a SHA-1 repository, in their stored order.
    fn stored_entries(bytes: &[u8]) -> Vec<StoredEntry> {
        let mut entries = Vec::new();
        let mut rest = bytes;
        while !rest.is_empty() {
            let space = rest
                .iter()
                .position(|&byte| byte == b' ')
                .expect("a mode ends at a space");
            let nul = rest
                .iter()
                .position(|&byte| byte == 0)
                .expect("a name ends at a NUL");
            entries.push(StoredEntry {
                mode: rest[..space].to_vec(),
                name: rest[space + 1..nul].to_vec(),
                id: rest[nul + 1..nul + 21].iter().fold(
                    String::with_capacity(40),
                    |mut hex, byte| {
                        write!(hex, "{byte:02x}").expect("write to a String");
                        hex
                    },
                ),
            });
            rest = &rest[nul + 21..];
        }
        entries
    }

    /// The entries of every tree of `manifest`.
    fn every_entry(manifest: &Manifest) -> Vec<StoredEntry> {
        manifest
            .trees
            .values()
            .flat_map(|bytes| stored_entries(bytes))
            .collect()
    }

    /// Whether `mode`, as a tree stores it, is a directory's.
    fn is_dir(mode: &[u8]) -> bool {
        std::str::from_utf8(mode)
            .ok()
            .and_then(|digits| u32::from_str_radix(digits, 8).ok())
            .is_some_and(|mode| mode & 0o170_000 == 0o040_000)
    }

    /// `entry`'s name as git's tree order compares it: a directory's with a `/` after it.
    fn order_key(entry: &StoredEntry) -> Vec<u8> {
        let mut key = entry.name.clone();
        if is_dir(&entry.mode) {
            key.push(b'/');
        }
        key
    }

    /// The header lines of every commit of `manifest`: each commit's lines up to its first
    /// empty one.
    fn header_lines(manifest: &Manifest) -> impl Iterator<Item = &[u8]> {
        manifest.commits.iter().flat_map(|commit| {
            commit
                .raw
                .split(|&byte| byte == b'\n')
                .take_while(|line| !line.is_empty())
        })
    }

    /// Whether `bytes` holds `part`.
    fn contains(bytes: &[u8], part: &[u8]) -> bool {
        bytes.windows(part.len()).any(|window| window == part)
    }

    /// The commit `oid` of `manifest`; one it lacks fails the test.
    fn commit_of<'m>(manifest: &'m Manifest, oid: &str) -> &'m ManifestCommit {
        manifest
            .commits
            .iter()
            .find(|commit| commit.oid == oid)
            .unwrap_or_else(|| panic!("no commit {oid} in the manifest"))
    }

    /// Every commit of `manifest` that `tip` reaches, `tip` among them.
    fn reaches(manifest: &Manifest, tip: &str) -> BTreeSet<String> {
        let parents: BTreeMap<&str, &[String]> = manifest
            .commits
            .iter()
            .map(|commit| (commit.oid.as_str(), commit.parents.as_slice()))
            .collect();
        let mut reached = BTreeSet::new();
        let mut stack = vec![tip.to_owned()];
        while let Some(oid) = stack.pop() {
            if reached.contains(&oid) {
                continue;
            }
            if let Some(&of) = parents.get(oid.as_str()) {
                stack.extend(of.iter().cloned());
            }
            reached.insert(oid);
        }
        reached
    }

    /// The files of the repository `repo`'s loose objects, `objects/<2 hex>/<38 hex>`.
    fn loose_objects(repo: &Path) -> Vec<PathBuf> {
        entries(&repo.join("objects"))
            .into_iter()
            .filter(|entry| entry.kind != EntryKind::Dir)
            .map(|entry| repo.join("objects").join(entry.path))
            .filter(|path| {
                path.parent()
                    .and_then(Path::file_name)
                    .and_then(|dir| dir.to_str())
                    .is_some_and(|dir| {
                        dir.len() == 2 && dir.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
            })
            .collect()
    }

    #[test]
    fn an_archive_extracts_to_the_same_files_twice() {
        let (first, second) = (extract_fixture("linear"), extract_fixture("linear"));
        assert_ne!(first.dir, second.dir);
        let files = |dir: &Path| -> Vec<(PathBuf, EntryKind, Vec<u8>)> {
            entries(dir)
                .into_iter()
                .map(|entry| {
                    let bytes = match entry.kind {
                        EntryKind::Dir => Vec::new(),
                        EntryKind::File { .. } => {
                            fs::read(dir.join(&entry.path)).expect("read a file")
                        }
                    };
                    (entry.path, entry.kind, bytes)
                })
                .collect()
        };
        let extracted = files(&first.dir);
        assert!(
            extracted
                .iter()
                .any(|(path, ..)| path == Path::new("repo.git/HEAD")),
            "{extracted:?}",
        );
        assert_eq!(extracted, files(&second.dir));
    }

    #[test]
    fn every_archive_is_sorted_with_fixed_metadata() {
        // The fixtures that have archives: the declared git's, whatever the fixtures at hand.
        let declared = declared_version();
        let archived = scripts()
            .into_iter()
            .filter(|name| too_old(&declared, &script_text(name)).is_none());
        for name in archived {
            let archive = archive_path(&name);
            let mut tar = tar::Archive::new(File::open(&archive).expect("open an archive"));
            let mut paths = Vec::new();
            for entry in tar.entries().expect("read an archive") {
                let entry = entry.expect("read an archive's entry");
                let header = entry.header();
                let path = entry.path().expect("an entry's path").into_owned();
                assert_eq!(header.mtime().unwrap(), 0, "{name}: {}", path.display());
                assert_eq!(header.uid().unwrap(), 0, "{name}: {}", path.display());
                assert_eq!(header.gid().unwrap(), 0, "{name}: {}", path.display());
                assert_eq!(
                    header.username().unwrap(),
                    Some(""),
                    "{name}: {}",
                    path.display()
                );
                assert_eq!(
                    header.groupname().unwrap(),
                    Some(""),
                    "{name}: {}",
                    path.display()
                );
                let mode = header.mode().unwrap();
                assert!(
                    mode == 0o644 || mode == 0o755,
                    "{name}: {} has mode {mode:o}",
                    path.display()
                );
                assert!(!left_out(&path), "{name}: {} is archived", path.display());
                paths.push(path);
            }
            assert!(paths.is_sorted(), "{name}: {paths:?}");
        }
    }

    #[test]
    fn check_names_every_difference_between_two_manifests() {
        let commit = |oid: &str, parents: &[&str]| ManifestCommit {
            oid: oid.to_owned(),
            parents: parents.iter().map(|parent| (*parent).to_owned()).collect(),
            raw: format!("tree t\nparents {parents:?}\n").into_bytes(),
            tree: "t".to_owned(),
        };
        let archived = Manifest {
            blobs: BTreeMap::from([("b1".to_owned(), 3)]),
            commits: vec![
                commit("c1", &[]),
                commit("c2", &["c1"]),
                commit("c3", &["c1"]),
            ],
            git_version: "git version 2.43.0".to_owned(),
            head: Some("refs/heads/main".to_owned()),
            object_format: "sha1".to_owned(),
            refs: BTreeMap::from([("refs/heads/main".to_owned(), "c2".to_owned())]),
            shallow: false,
            trees: BTreeMap::from([("t".to_owned(), b"tree bytes".to_vec())]),
        };
        let mut same = archived.clone();
        same.git_version = "git version 2.39.5".to_owned();
        let mut differences = Vec::new();
        compare_manifests(&mut differences, "x", Some(&archived), Some(&same));
        assert_eq!(differences, Vec::<String>::new(), "git_version is aside");

        let mut reordered = archived.clone();
        reordered.commits.swap(1, 2);
        compare_manifests(&mut differences, "x", Some(&archived), Some(&reordered));
        assert_eq!(
            differences,
            [
                "x: commits: the same commits in another order, from #1 on: the archive's c2, the rebuilt one's c3"
            ],
        );

        let mut rebuilt = archived.clone();
        rebuilt.head = None;
        rebuilt
            .refs
            .insert("refs/heads/main".to_owned(), "c3".to_owned());
        rebuilt.commits[2] = commit("c4", &["c2"]);
        rebuilt.trees.insert("t2".to_owned(), Vec::new());
        rebuilt.blobs.insert("b1".to_owned(), 4);
        let mut differences = Vec::new();
        compare_manifests(&mut differences, "x", Some(&archived), Some(&rebuilt));
        assert_eq!(
            differences,
            [
                "x: head: the archive's is Some(\"refs/heads/main\"), the rebuilt one's None",
                "x: ref refs/heads/main: the archive's is c2, the rebuilt one's c3",
                "x: commit c3: only in the archive",
                "x: commit c4: only in the rebuilt fixture",
                "x: tree t2: only in the rebuilt fixture",
                "x: blob b1: the archive's is 3 bytes, the rebuilt one's 4 bytes",
            ],
        );

        let mut differences = Vec::new();
        compare_manifests(&mut differences, "x", Some(&archived), None);
        compare_manifests(&mut differences, "y", None, None);
        assert_eq!(
            differences,
            ["x: the archive has a manifest, the rebuilt fixture none"]
        );
    }

    #[test]
    fn a_script_that_needs_a_newer_git_is_skipped() {
        let script = "# needs git 2.45\n# what it makes\n\n. \"$(dirname \"$0\")/lib.sh\"\n";
        assert_eq!(
            too_old("git version 2.43.0", script).as_deref(),
            Some("it needs git 2.45, and git version 2.43.0 is older"),
        );
        for version in ["git version 2.39.5", "git version 2.44.4"] {
            assert!(too_old(version, script).is_some(), "{version}");
        }
        // The release it names, and every later one, whatever follows their numbers.
        for version in [
            "git version 2.45.0",
            "git version 2.45.0.rc1",
            "git version 2.47.1 (Apple Git-154)",
            "git version 2.56.0",
            "git version 3.0.0",
        ] {
            assert_eq!(too_old(version, script), None, "{version}");
        }
        // It may follow other comment lines, but not a line that is not one.
        let second = "# what it makes\n# needs git 2.45\n\n. lib.sh\n";
        assert!(too_old("git version 2.43.0", second).is_some());
        for script in [
            "# what it makes\n\n. lib.sh\n",
            "# what it makes\n\n# needs git 2.45\n",
        ] {
            assert_eq!(too_old("git version 2.39.5", script), None, "{script:?}");
        }
        // The fixture set's own: `reftable.sh` needs git 2.45.
        let reftable = script_text("reftable");
        assert!(too_old("git version 2.43.0", &reftable).is_some());
        assert_eq!(too_old("git version 2.45.0", &reftable), None);
    }

    #[test]
    #[ignore = "rewrites crates/swarmops-testkit/git-fixtures with the declared git when SWARMOPS_REGEN_GIT_FIXTURES=1; run on purpose"]
    fn regenerate_git_fixtures() {
        if std::env::var_os(REGEN_ENV).is_none() {
            eprintln!("regenerate_git_fixtures: {REGEN_ENV} not set; nothing written");
            return;
        }
        regenerate(&scripts());
    }

    #[test]
    #[ignore = "rebuilds every fixture with the git it is given into $SWARMOPS_GIT_FIXTURES_CHECK and compares the manifests: the version matrix"]
    fn check_git_fixtures() {
        let Some(into) = std::env::var_os(CHECK_ENV) else {
            eprintln!("check_git_fixtures: {CHECK_ENV} not set; nothing checked");
            return;
        };
        let differences = check(Path::new(&into));
        assert!(
            differences.is_empty(),
            "the fixtures rebuilt into {} differ from the archives:\n{}",
            Path::new(&into).display(),
            differences.join("\n"),
        );
    }
}
