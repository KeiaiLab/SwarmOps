//! An install to update and a release to update it to, for the tests of `update`: the binary
//! replaces itself with a release it downloads through its built-in installer, and a test must
//! neither reach a release host nor touch an install outside its own temp directory.
//!
//! ```text
//!   <install>/an install/bin/   swarmops, so (copies of the binaries under test), .swarmops-receipt
//!
//!   so update ── SWARMOPS_CURL ───────────► the curl stand-in (`curl::FakeCurl`): "latest is v<L>"
//!      └─ /bin/sh, the installer ── PATH ─► <release>/bin/curl: -o FILE URL
//!                                             copies <release>/releases/<tag>/<file> to FILE,
//!                                             and records the run in <release>/calls/<n>/
//! ```
//!
//! [`Installed`] is the install: the two binaries copied into a directory whose path holds a
//! space, with the installer's receipt beside them. [`FakeRelease`] is what the installer's curl
//! finds: a directory first on `PATH` ([`FakeRelease::path`]) whose `curl` serves the release
//! files a test published ([`FakeRelease::publish`]) and nothing else, so that the installer,
//! which prefers curl, never runs the host's. The archive is a real one, in the release's
//! layout, of the binaries under test: packing and compressing them takes seconds, so it is
//! built once for a build of the binaries and kept under the system's temp directory.

use std::fs::{self, File};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, UNIX_EPOCH};

use tempfile::TempDir;

use crate::remote;

/// The installer's receipt, beside the binaries: `swarmops-ops`'s name for it, which this crate
/// cannot name by its constant.
pub const RECEIPT_FILE: &str = ".swarmops-receipt";

/// The lock file of an update, beside the binaries.
pub const LOCK_FILE: &str = ".swarmops-update.lock";

/// The note of an automatic update, beside the binaries.
pub const NOTE_FILE: &str = ".swarmops-update.note";

/// The directory a release is staged in, beside the binaries.
pub const STAGING_DIR: &str = ".swarmops-update.d";

/// A receipt that lets the binaries update themselves, as the installer writes it.
pub const RECEIPT_ON: &str = "receipt = 1\nauto_update = true\n";

/// A receipt that does not.
pub const RECEIPT_OFF: &str = "receipt = 1\nauto_update = false\n";

/// Where the releases are downloaded from, as the installer names it.
const DOWNLOAD_URL: &str = "https://github.com/KeiaiLab/SwarmOps/releases/download/";

/// The directory, under the system's temp directory, that keeps the archives built.
const CACHE: &str = "swarmops-test-releases";

/// How long the archives of another build of the binaries are kept.
const KEPT_FOR: Duration = Duration::from_hours(24);

/// The stand-in for the curl the installer runs: records the run, then copies the release file
/// its address names to the file `-o` names. Any other address is a host curl cannot resolve
/// (exit 6), and a file the release does not hold is an HTTP error (exit 22, as `-f` gives).
const SCRIPT: &str = r#"#!/bin/sh
dir=@DIR@
n=1
while ! /bin/mkdir "$dir/calls/$n" 2>/dev/null; do n=$((n + 1)); done
for arg in "$@"; do printf '%s\n' "$arg"; done > "$dir/calls/$n/args"
/usr/bin/env > "$dir/calls/$n/env"
out=
url=
while [ "$#" -gt 0 ]; do
  case $1 in
    -o) out=$2; shift 2 ;;
    --proto) shift 2 ;;
    -*) shift ;;
    *) url=$1; shift ;;
  esac
done
case $url in
  @URL@*) file=$dir/releases/${url#@URL@} ;;
  *) exit 6 ;;
esac
[ -f "$file" ] || exit 22
/bin/cp "$file" "$out"
"#;

/// An install in a temp directory of its own: copies of the two binaries, and a receipt.
#[derive(Debug)]
pub struct Installed {
    /// The temp directory, removed with the install.
    _tmp: TempDir,
    /// The install directory, every link of its path followed, as the binary names it.
    dir: PathBuf,
}

impl Installed {
    /// An install of `swarmops` and `so`, the two binaries by their paths, with a receipt that
    /// says `auto_update = true`, in a directory whose path holds a space.
    pub fn new(swarmops: &str, so: &str) -> Installed {
        let tmp = tempfile::tempdir().expect("a temp dir for an install");
        let dir = tmp.path().join("an install").join("bin");
        fs::create_dir_all(&dir).expect("make the install directory");
        let dir = dir.canonicalize().expect("the install directory's path");
        fs::copy(swarmops, dir.join("swarmops")).expect("copy swarmops");
        fs::copy(so, dir.join("so")).expect("copy so");
        let installed = Installed { _tmp: tmp, dir };
        installed.write(RECEIPT_FILE, RECEIPT_ON);
        installed
    }

    /// The install directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The directory as text.
    pub fn dir_text(&self) -> String {
        self.dir.to_str().expect("a UTF-8 temp dir").to_owned()
    }

    /// The path of the binary `name`, `swarmops` or `so`.
    pub fn bin(&self, name: &str) -> String {
        format!("{}/{name}", self.dir_text())
    }

    /// The path of the file `name` in the install directory.
    pub fn file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Writes `text` as the file `name` of the install directory.
    pub fn write(&self, name: &str, text: &str) {
        fs::write(self.file(name), text).expect("write a file beside the binaries");
    }

    /// What the file `name` of the install directory holds, or `None` when there is none.
    pub fn read(&self, name: &str) -> Option<String> {
        fs::read_to_string(self.file(name)).ok()
    }

    /// The names of everything in the install directory, in order.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.dir)
            .expect("read the install directory")
            .map(|entry| entry.expect("an entry").file_name())
            .map(|name| name.into_string().expect("a UTF-8 name"))
            .collect();
        names.sort_unstable();
        names
    }

    /// Which files the two names are: each one's inode, size and modification time. A rename
    /// over a name changes its inode, so equal values mean that nothing was replaced.
    pub fn identity(&self) -> [(u64, u64, i64); 2] {
        ["swarmops", "so"].map(|name| {
            let file = fs::metadata(self.file(name)).expect("an installed binary");
            (file.ino(), file.len(), file.mtime_nsec())
        })
    }
}

/// One run of the installer's curl stand-in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Download {
    /// Its arguments.
    pub args: Vec<String>,
    /// The names of the variables it ran with, the shell's own among them.
    pub env: Vec<String>,
}

impl Download {
    /// The address asked for: the last argument.
    pub fn url(&self) -> &str {
        self.args.last().map_or("", String::as_str)
    }
}

/// The releases a test published, and the `curl` that serves them to the installer.
#[derive(Debug)]
pub struct FakeRelease {
    /// The stand-in (`bin/curl`), the release files (`releases/<tag>/`) and its runs (`calls/`).
    dir: TempDir,
}

impl Default for FakeRelease {
    fn default() -> FakeRelease {
        FakeRelease::new()
    }
}

impl FakeRelease {
    /// A stand-in that serves no release yet.
    pub fn new() -> FakeRelease {
        let dir = tempfile::tempdir().expect("a temp dir for a release");
        for sub in ["bin", "releases", "calls"] {
            fs::create_dir(dir.path().join(sub)).expect("make the release's directory");
        }
        let at = dir.path().to_str().expect("a UTF-8 temp dir");
        let script = SCRIPT
            .replace("@DIR@", &remote::sh_quote(at))
            .replace("@URL@", DOWNLOAD_URL);
        remote::write_script(&dir.path().join("bin").join("curl"), &script);
        FakeRelease { dir }
    }

    /// The value for `PATH`: the stand-in's directory, then this process's `PATH`, where the
    /// installer finds its other tools.
    pub fn path(&self) -> String {
        let bin = self.dir.path().join("bin");
        let host = std::env::var("PATH").expect("a UTF-8 PATH");
        format!("{}:{host}", bin.to_str().expect("a UTF-8 temp dir"))
    }

    /// Publishes release `version`: the archive for this host's target, holding the binaries
    /// `swarmops` and `so` in the release's layout, and a `SHA256SUMS` with its line.
    pub fn publish(&self, version: &str, swarmops: &str, so: &str) -> &FakeRelease {
        let name = archive_name(version);
        let (archive, digest) = cached_archive(version, Path::new(swarmops), Path::new(so));
        let release = self.release_dir(version);
        fs::create_dir_all(&release).expect("make the release's directory");
        let served = release.join(&name);
        // Published again, the name is linked again: writing through it would write the
        // archive every other test's release links to.
        let _ = fs::remove_file(&served);
        if fs::hard_link(&archive, &served).is_err() {
            fs::copy(&archive, &served).expect("copy the archive");
        }
        self.sums(version, &digest)
    }

    /// Replaces release `version`'s `SHA256SUMS` by one whose line for the archive names
    /// another digest: the download no longer matches it.
    pub fn mismatch(&self, version: &str) -> &FakeRelease {
        self.sums(version, &"0".repeat(64))
    }

    /// Writes release `version`'s `SHA256SUMS`: `digest` for this host's archive.
    fn sums(&self, version: &str, digest: &str) -> &FakeRelease {
        let line = format!("{digest}  {}\n", archive_name(version));
        fs::write(self.release_dir(version).join("SHA256SUMS"), line).expect("write SHA256SUMS");
        self
    }

    /// Where release `version`'s files are served from.
    fn release_dir(&self, version: &str) -> PathBuf {
        self.dir.path().join("releases").join(format!("v{version}"))
    }

    /// The runs of the stand-in so far, in order.
    pub fn downloads(&self) -> Vec<Download> {
        let calls = self.dir.path().join("calls");
        let mut numbers: Vec<usize> = fs::read_dir(&calls)
            .expect("read the stand-in's runs")
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
            .collect();
        numbers.sort_unstable();
        let lines = |file: PathBuf| -> Vec<String> {
            let text = fs::read_to_string(file).expect("read a run's record");
            text.lines().map(str::to_owned).collect()
        };
        numbers
            .into_iter()
            .map(|n| {
                let call = calls.join(n.to_string());
                Download {
                    args: lines(call.join("args")),
                    env: lines(call.join("env"))
                        .iter()
                        .filter_map(|line| line.split_once('='))
                        .map(|(name, _)| name.to_owned())
                        .collect(),
                }
            })
            .collect()
    }
}

/// The release target of this host, as the installer picks it.
pub fn target() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", _) => "aarch64-apple-darwin",
        (_, "x86_64") => "x86_64-unknown-linux-gnu",
        _ => "aarch64-unknown-linux-gnu",
    }
}

/// The name of release `version`'s archive for this host.
pub fn archive_name(version: &str) -> String {
    format!("swarmops-{version}-{}.tar.gz", target())
}

/// The archive of release `version` holding the binaries `swarmops` and `so`, and its SHA-256,
/// built once for these files as they are (their sizes and modification times) and kept: a
/// lock file holds off the other test processes while one builds, and the archives of other
/// builds of the binaries are removed once they are [`KEPT_FOR`] old.
fn cached_archive(version: &str, swarmops: &Path, so: &Path) -> (PathBuf, String) {
    let cache = std::env::temp_dir().join(CACHE);
    fs::create_dir_all(&cache).expect("make the archives' directory");
    let lock = File::create(cache.join("lock")).expect("open the archives' lock");
    lock.lock().expect("lock the archives");

    // 1. The directory of this build of the binaries; any other build's goes.
    let stamp = |path: &Path| {
        let file = fs::metadata(path).expect("a binary under test");
        let modified = file.modified().expect("its modification time");
        let since = modified.duration_since(UNIX_EPOCH).expect("after 1970");
        format!("{}-{}", file.len(), since.as_nanos())
    };
    let key = format!("{}-{}", stamp(swarmops), stamp(so));
    for entry in fs::read_dir(&cache).expect("read the archives' directory") {
        let entry = entry.expect("an entry");
        let old = entry
            .metadata()
            .and_then(|found| found.modified())
            .is_ok_and(|at| at.elapsed().is_ok_and(|age| age > KEPT_FOR));
        if entry.path().is_dir() && entry.file_name() != *key && old {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
    let kept = cache.join(&key);
    fs::create_dir_all(&kept).expect("make this build's directory");

    // 2. The archive and its digest, built unless both are there.
    let archive = kept.join(format!("{version}.tar.gz"));
    let digest_file = kept.join(format!("{version}.sha256"));
    if let (true, Ok(digest)) = (archive.is_file(), fs::read_to_string(&digest_file)) {
        return (archive, digest);
    }
    let work = tempfile::tempdir_in(&kept).expect("a directory to pack in");
    let top = format!("swarmops-{version}-{}", target());
    let packed = work.path().join(&top);
    fs::create_dir(&packed).expect("make the archive's directory");
    fs::copy(swarmops, packed.join("swarmops")).expect("copy swarmops");
    fs::copy(so, packed.join("so")).expect("copy so");
    let staged = work.path().join("archive");
    sh(
        r#"tar -cf - -C "$1" "$2" | gzip -1 > "$3""#,
        &[work.path(), Path::new(&top), &staged],
    );
    let summed = sh(
        r#"if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi"#,
        &[&staged],
    );
    let digest = summed.split_whitespace().next().expect("a digest");
    assert_eq!(digest.len(), 64, "a SHA-256 in hex: {summed}");
    fs::rename(&staged, &archive).expect("keep the archive");
    fs::write(&digest_file, digest).expect("keep the digest");
    (archive, digest.to_owned())
}

/// Runs `script` under `sh` with `args`; what it printed. A failure fails the test.
fn sh(script: &str, args: &[&Path]) -> String {
    let output = Command::new("sh")
        .args(["-c", script, "sh"])
        .args(args)
        .output()
        .expect("start sh");
    assert!(output.status.success(), "{script}: {output:?}");
    String::from_utf8(output.stdout).expect("UTF-8 output")
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    /// A stand-in for a binary, small enough to pack in no time: it prints `text`.
    fn program(dir: &Path, name: &str, text: &str) -> String {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\necho '{text}'\n")).expect("write a program");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("set its mode");
        path.to_str().expect("a UTF-8 path").to_owned()
    }

    /// Runs the stand-in as the installer runs its curl: `-o out url`, and no variable.
    fn fetch(release: &FakeRelease, url: &str, out: &Path) -> Option<i32> {
        Command::new(release.dir.path().join("bin").join("curl"))
            .args(["-fsSL", "--proto", "=https", "--tlsv1.2", "-o"])
            .arg(out)
            .arg(url)
            .env_clear()
            .env("KEPT", "1")
            .status()
            .expect("run the stand-in")
            .code()
    }

    #[test]
    fn the_stand_in_serves_what_was_published_and_nothing_else() {
        let tmp = tempfile::tempdir().unwrap();
        let swarmops = program(tmp.path(), "swarmops", "swarmops 7.7.7");
        let so = program(tmp.path(), "so", "swarmops 7.7.7");
        let release = FakeRelease::new();
        release.publish("7.7.7", &swarmops, &so);
        let name = archive_name("7.7.7");
        let base = format!("{DOWNLOAD_URL}v7.7.7");
        let out = tmp.path().join("out");

        // 1. The archive and its line; the digest is the archive's.
        assert_eq!(
            fetch(&release, &format!("{base}/SHA256SUMS"), &out),
            Some(0)
        );
        let sums = fs::read_to_string(&out).unwrap();
        assert!(sums.ends_with(&format!("  {name}\n")), "{sums}");
        let archive = tmp.path().join(&name);
        assert_eq!(
            fetch(&release, &format!("{base}/{name}"), &archive),
            Some(0)
        );
        fs::copy(&out, tmp.path().join("SHA256SUMS")).unwrap();
        let checked = sh(
            r#"cd "$1" && if command -v sha256sum >/dev/null 2>&1; then sha256sum -c SHA256SUMS; else shasum -a 256 -c SHA256SUMS; fi"#,
            &[tmp.path()],
        );
        assert!(checked.contains("OK"), "{checked}");

        // 2. The release's layout: one directory, the two binaries in it.
        let listed = sh(r#"tar -tzf "$1""#, &[&archive]);
        let top = format!("swarmops-7.7.7-{}", target());
        for member in ["swarmops", "so"] {
            assert!(listed.contains(&format!("{top}/{member}\n")), "{listed}");
        }

        // 3. Nothing else: another file, another release, another host.
        assert_eq!(fetch(&release, &format!("{base}/missing"), &out), Some(22));
        let other = format!("{DOWNLOAD_URL}v8.0.0/SHA256SUMS");
        assert_eq!(fetch(&release, &other, &out), Some(22));
        assert_eq!(fetch(&release, "https://example.com/x", &out), Some(6));

        // 4. Each run is recorded, with what it ran with.
        let downloads = release.downloads();
        assert_eq!(downloads.len(), 5);
        assert_eq!(downloads[0].url(), format!("{base}/SHA256SUMS"));
        assert_eq!(downloads[4].url(), "https://example.com/x");
        assert!(downloads[0].env.contains(&"KEPT".to_owned()));
        assert!(!downloads[0].env.contains(&"HOME".to_owned()));

        // 5. A SHA256SUMS that names another digest for the archive.
        release.mismatch("7.7.7");
        assert_eq!(
            fetch(&release, &format!("{base}/SHA256SUMS"), &out),
            Some(0)
        );
        let wrong = fs::read_to_string(&out).unwrap();
        assert_eq!(wrong, format!("{}  {name}\n", "0".repeat(64)));
        assert!(
            release
                .path()
                .starts_with(release.dir.path().to_str().unwrap())
        );
    }

    #[test]
    fn an_install_is_two_binaries_and_a_receipt() {
        let tmp = tempfile::tempdir().unwrap();
        let swarmops = program(tmp.path(), "a", "swarmops 7.7.7");
        let so = program(tmp.path(), "b", "swarmops 7.7.7");
        let install = Installed::new(&swarmops, &so);

        assert_eq!(install.names(), [RECEIPT_FILE, "so", "swarmops"]);
        assert_eq!(install.read(RECEIPT_FILE).as_deref(), Some(RECEIPT_ON));
        assert!(install.dir_text().contains("an install"), "a space in it");
        assert_eq!(install.dir(), install.dir().canonicalize().unwrap());
        let ran = Command::new(install.bin("so")).output().unwrap();
        assert_eq!(ran.stdout, b"swarmops 7.7.7\n");

        // A file renamed over a name is another file; the same file is the same.
        let before = install.identity();
        assert_eq!(install.identity(), before);
        let staged = install.file("staged");
        fs::copy(&so, &staged).unwrap();
        fs::rename(&staged, install.file("so")).unwrap();
        let after = install.identity();
        assert_eq!(after[0], before[0]);
        assert_ne!(after[1], before[1]);

        install.write(RECEIPT_FILE, RECEIPT_OFF);
        assert_eq!(install.read(RECEIPT_FILE).as_deref(), Some(RECEIPT_OFF));
        assert_eq!(install.read(NOTE_FILE), None);
    }
}
