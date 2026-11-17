//! `update` through the binary: the program replaced by a newer release, the refusals, the
//! switch of automatic update, and what `update --background` does where automatic update is
//! on. No test reaches a release host, and none touches an install but its own.
//!
//! ```text
//!   <tmp>/an install/bin/{swarmops,so}   copies of the binaries under test, with a receipt
//!   SWARMOPS_TEST_UPDATE_RELEASE=1       the copies count as a release's build
//!   SWARMOPS_TEST_VERSION=0.0.1          and as an old one, where a test says so
//!   SWARMOPS_CURL = the curl stand-in    "the latest release is v<this build's version>"
//!   PATH = the release stand-in first    the built-in installer downloads the archive of the
//!                                        binaries under test, packed as a release packs them
//! ```
//!
//! The installed copies answer `--version` with the hook's version, and the staged ones, which
//! `update` runs without any `SWARMOPS_*` variable, with their own: so the release is newer than
//! what is installed, and is what it says it is. Both hooks exist only in a build with the
//! `test-hooks` feature, and so does this file (`all.rs`); the release proof (`test_hooks.rs`)
//! shows a build without it ignoring them.
//!
//! Every proxy variable names a closed port of this host: a run that did reach for the network,
//! against everything above, would be refused there.

use std::fs::{self, File};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

use serde_json::{Value, json};
use swarmops_testkit::curl::{CURL_ENV, FakeCurl};
use swarmops_testkit::release::{
    self, FakeRelease, Installed, LOCK_FILE, NOTE_FILE, RECEIPT_FILE, RECEIPT_OFF, RECEIPT_ON,
    STAGING_DIR,
};
use tempfile::TempDir;

use crate::support::{BIN, Run, SO, run_bin};

/// This build's version: what the binaries under test print, and so the release they are.
const REAL: &str = env!("CARGO_PKG_VERSION");

/// The version the installed copies say they are, where a test makes them old.
const OLD: &str = "0.0.1";

/// The hook that makes the binary print a version for its own.
const VERSION_HOOK: &str = "SWARMOPS_TEST_VERSION";

/// The hook that makes the binary count as a release's build.
const RELEASE_HOOK: &str = "SWARMOPS_TEST_UPDATE_RELEASE";

/// The variable that turns automatic update on or off.
const AUTO_UPDATE: &str = "SWARMOPS_AUTO_UPDATE";

/// Where no proxy listens: the discard port of this host.
const NO_PROXY_THERE: &str = "http://127.0.0.1:9";

/// Where the releases are, as the binary asks for them.
const RELEASES: &str = "https://github.com/KeiaiLab/SwarmOps/releases";

/// An install of the binaries under test, the releases its installer finds, and the curl the
/// binary asks for the latest release with.
struct Site {
    install: Installed,
    release: FakeRelease,
    curl: FakeCurl,
    /// An empty `HOME`, and `PATH` with the release stand-in first.
    home: TempDir,
    path: String,
}

impl Site {
    fn new() -> Site {
        let release = FakeRelease::new();
        Site {
            install: Installed::new(BIN, SO),
            path: release.path(),
            release,
            curl: FakeCurl::new(),
            home: tempfile::tempdir().expect("a temp HOME"),
        }
    }

    /// Runs the installed binary `name` with `args`, as a release's build, with `env` over the
    /// site's own variables. Whatever it does, it prints one envelope and nothing on stderr.
    fn run(&self, name: &str, args: &[&str], env: &[(&str, &str)]) -> Run {
        let curl = self.curl.path();
        let mut vars = vec![
            (RELEASE_HOOK, "1"),
            (CURL_ENV, curl.to_str().expect("a UTF-8 path")),
            ("PATH", self.path.as_str()),
            ("HOME", self.home.path().to_str().expect("a UTF-8 path")),
        ];
        for proxy in ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"] {
            vars.push((proxy, NO_PROXY_THERE));
        }
        vars.extend_from_slice(env);
        let run = run_bin(&self.install.bin(name), args, &vars, "");
        assert_eq!(run.stderr, "", "nothing on stderr: {run}");
        run.envelope();
        run
    }

    /// Queues the answer that the latest release is `version`.
    fn latest_is(&self, version: &str) {
        self.curl.print(&format!("{RELEASES}/tag/v{version}\n302"));
    }

    /// Queues the answer to a request for a mark: the release holds it, or does not.
    fn mark_is(&self, there: bool) {
        if there {
            self.curl.print("https://release-assets.example/mark\n302");
        } else {
            self.curl.print("\n404");
        }
    }

    /// Publishes release `version`, holding the binaries under test.
    fn publish(&self, version: &str) {
        self.release.publish(version, BIN, SO);
    }
}

/// What a site's install updates itself as when nothing but the caller stops it: `on`, or
/// `off_owner` where the tests run as root, whose installs never do.
fn on_for_this_user() -> &'static str {
    if rustix::process::geteuid().is_root() {
        "off_owner"
    } else {
        "on"
    }
}

/// Whether the tests of automatic update can run: not as root. Says so on stderr when not.
fn automatic_update_can_be_on() -> bool {
    let can = on_for_this_user() == "on";
    if !can {
        eprintln!("skipped: root's installs never update themselves");
    }
    can
}

/// After one `--check` and one `update` to [`REAL`]: each made one request of the binary's own,
/// for the latest release, and the installer downloaded that release's two files. Neither the
/// curl nor the installer's downloader got a `SWARMOPS_*` variable.
fn one_look_each_and_the_release_s_two_files(site: &Site) {
    let asked = site.curl.calls();
    assert_eq!(asked.len(), 2, "one request for each look");
    for call in &asked {
        assert!(
            call.stdin
                .contains(&format!("url = \"{RELEASES}/latest\"\n"))
        );
        assert!(call.stdin.contains("User-Agent: swarmops-update"));
        assert!(
            !call.env.iter().any(|name| name.starts_with("SWARMOPS_")),
            "{:?}",
            call.env
        );
    }
    let downloads = site.release.downloads();
    let mut urls: Vec<&str> = downloads.iter().map(release::Download::url).collect();
    urls.sort_unstable();
    let base = format!("{RELEASES}/download/v{REAL}");
    assert_eq!(
        urls,
        [
            format!("{base}/SHA256SUMS"),
            format!("{base}/{}", release::archive_name(REAL)),
        ]
    );
    for download in &downloads {
        assert!(
            !download
                .env
                .iter()
                .any(|name| name.starts_with("SWARMOPS_")),
            "{:?}",
            download.env
        );
    }
}

#[test]
fn update_replaces_both_names_with_a_newer_release() {
    let site = Site::new();
    site.publish(REAL);
    let old = [(VERSION_HOOK, OLD)];
    let before = site.install.identity();
    let dir = site.install.dir_text();

    // 1. --check looks and installs nothing: no lock, no download.
    site.latest_is(REAL);
    let checked = site.run("so", &["update", "--check"], &old).ok();
    assert_eq!(
        checked,
        json!({
            "current": OLD,
            "latest": REAL,
            "updated": false,
            "auto_update": "off_env",
            "dir": dir,
            "notes": format!("{RELEASES}/tag/v{REAL}"),
        })
    );
    assert_eq!(site.install.identity(), before);
    assert_eq!(site.install.names(), [RECEIPT_FILE, "so", "swarmops"]);
    assert_eq!(site.release.downloads(), []);

    // 2. update installs it, over what an earlier update left, whatever the caller's
    //    variables name: the installer gets none of them.
    fs::create_dir(site.install.file(STAGING_DIR)).expect("a leftover staging directory");
    site.install
        .write(&format!("{STAGING_DIR}/swarmops"), "half a download");
    site.install.write(NOTE_FILE, "available 0.0.2\n");
    site.latest_is(REAL);
    let pins = [
        old[0],
        ("SWARMOPS_VERSION", "9.9.9"),
        ("SWARMOPS_INSTALL_DIR", "/nonexistent/swarmops-test"),
        ("SWARMOPS_DOWNLOAD_BASE", "https://example.com/download"),
        ("SWARMOPS_LATEST_URL", "https://example.com/latest"),
    ];
    let updated = site.run("so", &["update"], &pins).ok();
    assert_eq!(
        updated,
        json!({
            "current": REAL,
            "previous": OLD,
            "latest": REAL,
            "updated": true,
            "auto_update": "off_env",
            "dir": dir,
            "notes": format!("{RELEASES}/tag/v{REAL}"),
        })
    );
    let after = site.install.identity();
    assert_ne!(after[0].0, before[0].0, "swarmops is another file");
    assert_ne!(after[1].0, before[1].0, "so is another file");
    assert_eq!(
        site.install.names(),
        [RECEIPT_FILE, LOCK_FILE, "so", "swarmops"],
        "the staging directory and the note are gone"
    );
    assert_eq!(site.install.read(RECEIPT_FILE).as_deref(), Some(RECEIPT_ON));
    for name in ["swarmops", "so"] {
        let mode = fs::metadata(site.install.file(name))
            .expect("a binary")
            .mode();
        assert_eq!(mode & 0o777, 0o755, "{name}");
        let version = run_bin(&site.install.bin(name), &["--version"], &[], "");
        assert_eq!(version.stdout, format!("swarmops {REAL}\n"), "{name}");
    }

    // 3. What was asked of whom.
    one_look_each_and_the_release_s_two_files(&site);

    // 4. A second run, by the other name, finds the install current and replaces nothing.
    site.latest_is(REAL);
    let current = site.run("swarmops", &["update"], &[]).ok();
    assert_eq!(current["updated"], false);
    assert_eq!(
        (&current["current"], &current["latest"]),
        (&json!(REAL), &json!(REAL))
    );
    assert!(current.get("previous").is_none(), "{current}");
    assert_eq!(site.install.identity(), after);
    assert_eq!(
        site.release.downloads().len(),
        2,
        "nothing more was downloaded"
    );
}

#[test]
fn a_latest_release_that_is_not_newer_installs_nothing() {
    let site = Site::new();
    let before = site.install.identity();
    for (installed, latest) in [
        ("5.0.0", "4.9.9"),
        ("5.0.0", "5.0.0"),
        ("5.0.0-rc.1", "4.9.9"),
    ] {
        site.latest_is(latest);
        let result = site
            .run("so", &["update"], &[(VERSION_HOOK, installed)])
            .ok();
        assert_eq!(result["updated"], false, "{installed} to {latest}");
        assert_eq!(result["current"], installed);
        assert_eq!(result["latest"], latest);
        assert!(result.get("previous").is_none(), "{result}");
    }
    assert_eq!(site.install.identity(), before);
    assert_eq!(site.release.downloads(), []);
    assert!(!site.install.file(STAGING_DIR).exists());
}

#[test]
fn an_answer_that_names_no_release_is_unreachable() {
    let site = Site::new();
    let before = site.install.identity();
    let old = [(VERSION_HOOK, OLD)];

    // 1. Answers that are no redirect to a release's page: a pre-release's page, another
    //    host's, a page that answers 200, a redirect without an address, and no answer read.
    let tag = format!("{RELEASES}/tag");
    let answers = [
        (format!("{tag}/v9.9.9-rc.1\n302"), json!(302)),
        (
            "https://example.com/KeiaiLab/SwarmOps/releases/tag/v9.9.9\n302".to_owned(),
            json!(302),
        ),
        (format!("{tag}/v9.9.9\n200"), json!(200)),
        ("\n302".to_owned(), json!(302)),
        ("<html>".to_owned(), Value::Null),
    ];
    for (printed, status) in &answers {
        for form in [&["update"][..], &["update", "--check"]] {
            site.curl.print(printed);
            let error = site.run("so", form, &old).err("E_REMOTE_UNREACHABLE");
            assert_eq!(error["exit"], 4, "{printed:?}");
            assert_eq!(error["details"]["phase"], "update");
            assert_eq!(error["details"]["host"], "github.com");
            let said = error["details"].get("http_status").unwrap_or(&Value::Null);
            assert_eq!(said, status, "{printed:?}");
            assert!(error["details"].get("curl_exit").is_none(), "{error}");
        }
    }

    // 2. A curl that reached no answer: its exit code, and the cause it names.
    for (exit, cause) in [(6, "dns"), (7, "connect"), (28, "timeout"), (60, "tls")] {
        site.curl.fail(exit);
        let error = site
            .run("so", &["update"], &old)
            .err("E_REMOTE_UNREACHABLE");
        assert_eq!(error["details"]["curl_exit"], exit);
        assert_eq!(error["details"]["cause"], cause);
        assert_eq!(error["retryable"], true);
    }

    assert_eq!(site.install.identity(), before);
    assert_eq!(site.release.downloads(), []);
}

#[test]
fn a_download_that_fails_its_check_replaces_nothing() {
    let site = Site::new();
    let before = site.install.identity();
    let old = [(VERSION_HOOK, OLD)];

    // 1. A release whose archive is not what its SHA256SUMS says.
    site.publish(REAL);
    site.release.mismatch(REAL);
    site.latest_is(REAL);
    let error = site.run("so", &["update"], &old).err("E_IO");
    assert_eq!(error["exit"], 1);
    assert_eq!(error["details"]["step"], "install");
    assert_eq!(error["details"]["installer_exit"], 1);
    let said = error["details"]["installer"]
        .as_str()
        .expect("its last line");
    assert!(
        said.contains("does not match its line in SHA256SUMS"),
        "{said}"
    );
    let message = error["message"].as_str().expect("a message");
    assert!(message.ends_with("nothing was replaced"), "{message}");
    assert_eq!(site.install.identity(), before);
    assert_eq!(
        site.install.names(),
        [RECEIPT_FILE, LOCK_FILE, "so", "swarmops"]
    );

    // 2. A release that holds no archive at all.
    site.latest_is("77.0.0");
    let error = site.run("so", &["update"], &old).err("E_IO");
    assert_eq!(error["details"]["step"], "install");
    assert_eq!(site.install.identity(), before);
    assert!(!site.install.file(STAGING_DIR).exists());
}

#[test]
fn a_staged_binary_that_is_not_the_release_replaces_nothing() {
    // The archive of "99.0.0" holds the binaries under test, which say what they are.
    let site = Site::new();
    let before = site.install.identity();
    site.publish("99.0.0");
    site.latest_is("99.0.0");
    let error = site
        .run("so", &["update"], &[(VERSION_HOOK, OLD)])
        .err("E_IO");
    assert_eq!(error["details"]["step"], "verify");
    assert_eq!(error["details"]["expected"], "swarmops 99.0.0");
    let found = error["details"]["found"].as_str().expect("what it printed");
    assert!(found.starts_with(&format!("swarmops {REAL}")), "{found}");
    assert_eq!(site.install.identity(), before);
    assert_eq!(
        site.install.names(),
        [RECEIPT_FILE, LOCK_FILE, "so", "swarmops"],
        "the staging directory is gone"
    );
    assert_eq!(
        site.release.downloads().len(),
        2,
        "it was downloaded and checked"
    );
}

#[test]
fn an_install_update_cannot_replace_is_refused_before_any_request() {
    let site = Site::new();
    let before = site.install.identity();
    let dir = site.install.dir_text();
    let refusal = |run: Run, reason: &str| {
        let error = run.err("E_INVALID_ARG");
        assert_eq!(error["exit"], 2, "{reason}");
        assert_eq!(error["details"]["reason"], reason);
        assert!(error["hint"].is_string(), "{reason}");
        error
    };

    // 1. No curl to ask with: nothing was locked.
    let nowhere = [(CURL_ENV, "/nonexistent/swarmops-test/curl")];
    refusal(site.run("so", &["update"], &nowhere), "curl_missing");
    refusal(
        site.run("so", &["update", "--check"], &nowhere),
        "curl_missing",
    );
    assert_eq!(site.install.names(), [RECEIPT_FILE, "so", "swarmops"]);

    // 2. Another update holds the lock: retryable, and no request was made.
    let lock = File::create(site.install.file(LOCK_FILE)).expect("make the lock file");
    lock.lock().expect("hold the lock");
    let busy = site.run("so", &["update"], &[]).err("E_BUSY");
    assert_eq!(
        (&busy["exit"], &busy["retryable"]),
        (&json!(4), &json!(true))
    );
    assert_eq!(busy["details"]["reason"], "update_running");
    drop(lock);

    // 3. A file that is no receipt: refused by every form, and left as it is.
    let foreign = "receipt = 2\nauto_update = true\n";
    site.install.write(RECEIPT_FILE, foreign);
    for form in [
        &["update"][..],
        &["update", "--check"],
        &["update", "--status"],
        &["update", "--auto", "off"],
        &["update", "--auto", "on"],
        &["update", "--background"],
    ] {
        let error = refusal(site.run("so", form, &[]), "receipt");
        let path = format!("{dir}/{RECEIPT_FILE}");
        assert_eq!(error["details"]["path"], path, "{form:?}");
    }
    assert_eq!(site.install.read(RECEIPT_FILE).as_deref(), Some(foreign));

    // 4. No receipt: a package's install, or an archive unpacked by hand.
    fs::remove_file(site.install.file(RECEIPT_FILE)).expect("remove the receipt");
    for form in [
        &["update"][..],
        &["update", "--check"],
        &["update", "--auto", "on"],
    ] {
        let error = refusal(site.run("so", form, &[]), "no_receipt");
        assert_eq!(error["details"]["dir"], dir, "{form:?}");
    }
    let off = site.run("so", &["update", "--auto", "off"], &[]).ok();
    assert_eq!(
        off,
        json!({"current": REAL, "updated": false, "auto_update": "off_no_receipt"})
    );
    let status = site.run("so", &["update", "--status"], &[]).ok();
    assert_eq!(status, off);
    assert!(
        !site.install.file(RECEIPT_FILE).exists(),
        "none was written"
    );

    // 5. No release's build, with a receipt all the same: a build from source that was copied
    //    over an install.
    site.install.write(RECEIPT_FILE, RECEIPT_ON);
    let source = [(RELEASE_HOOK, "0")];
    for form in [
        &["update"][..],
        &["update", "--check"],
        &["update", "--auto", "on"],
    ] {
        refusal(site.run("so", form, &source), "not_a_release_build");
    }
    let status = site.run("so", &["update", "--status"], &source).ok();
    assert_eq!(status["auto_update"], "off_source_build");
    assert_eq!(status["dir"], dir, "the receipt was found");

    // Not one request, and nothing replaced.
    assert_eq!(site.curl.calls(), []);
    assert_eq!(site.release.downloads(), []);
    assert_eq!(site.install.identity(), before);
    assert_eq!(site.install.read(RECEIPT_FILE).as_deref(), Some(RECEIPT_ON));
}

#[test]
fn auto_writes_the_receipt_and_status_says_what_decides() {
    let site = Site::new();
    let dir = site.install.dir_text();
    let on = [(AUTO_UPDATE, "on")];
    let status = |env: &[(&str, &str)]| site.run("so", &["update", "--status"], env).ok();

    // 1. As installed: the receipt says true, and the caller's variable decides.
    assert_eq!(
        status(&[])["auto_update"],
        "off_env",
        "the harness turns it off"
    );
    assert_eq!(status(&[(AUTO_UPDATE, "never")])["auto_update"], "off_env");
    assert_eq!(status(&on)["auto_update"], on_for_this_user());
    assert_eq!(
        status(&on),
        json!({
            "current": REAL,
            "updated": false,
            "auto_update": on_for_this_user(),
            "dir": dir,
        })
    );
    let ci = status(&[(AUTO_UPDATE, ""), ("CI", "true")]);
    assert_eq!(ci["auto_update"], "off_ci");

    // 2. Off: the receipt is rewritten, and decides before the variable does.
    let off = site.run("so", &["update", "--auto", "off"], &on).ok();
    assert_eq!(off["auto_update"], "off");
    assert_eq!(off["updated"], false);
    assert_eq!(
        site.install.read(RECEIPT_FILE).as_deref(),
        Some(RECEIPT_OFF)
    );
    let mode = fs::metadata(site.install.file(RECEIPT_FILE))
        .expect("the receipt")
        .mode();
    assert_eq!(mode & 0o777, 0o644, "the installer's mode");
    assert_eq!(status(&on)["auto_update"], "off");
    assert_eq!(site.install.names(), [RECEIPT_FILE, "so", "swarmops"]);

    // Off again changes no file.
    let written = fs::metadata(site.install.file(RECEIPT_FILE))
        .expect("the receipt")
        .ino();
    site.run("swarmops", &["update", "--auto", "off"], &on).ok();
    let kept = fs::metadata(site.install.file(RECEIPT_FILE))
        .expect("the receipt")
        .ino();
    assert_eq!(kept, written, "the receipt was not written again");

    // 3. On again, with the keys this build does not know kept.
    let with_more = "receipt = 1\nauto_update = false\nchannel = \"stable\"\n";
    site.install.write(RECEIPT_FILE, with_more);
    let again = site.run("so", &["update", "--auto", "on"], &on).ok();
    assert_eq!(again["auto_update"], on_for_this_user());
    assert_eq!(
        site.install.read(RECEIPT_FILE).as_deref(),
        Some("receipt = 1\nauto_update = true\nchannel = \"stable\"\n")
    );

    // 4. A receipt that cannot be replaced: the directory is not this user's to write.
    if on_for_this_user() == "on" {
        let closed = fs::Permissions::from_mode(0o555);
        fs::set_permissions(site.install.dir(), closed).expect("close the directory");
        let error = site
            .run("so", &["update", "--auto", "off"], &on)
            .err("E_IO");
        assert_eq!(error["details"]["step"], "receipt");
        let locked = site.run("so", &["update"], &on).err("E_IO");
        assert_eq!(locked["details"]["step"], "lock");
        let open = fs::Permissions::from_mode(0o755);
        fs::set_permissions(site.install.dir(), open).expect("open the directory");
        assert_eq!(
            status(&on)["auto_update"],
            "on",
            "the receipt still says true"
        );
    }

    // 5. The install reached through a link is the same install.
    let link = site.home.path().join("link");
    std::os::unix::fs::symlink(site.install.file("so"), &link).expect("link the binary");
    let env = [(RELEASE_HOOK, "1")];
    let linked = run_bin(
        link.to_str().expect("a UTF-8 path"),
        &["update", "--status"],
        &env,
        "",
    );
    assert_eq!(linked.ok()["dir"], dir);

    // None of it asked anything of anyone.
    assert_eq!(site.curl.calls(), []);
    assert_eq!(site.release.downloads(), []);
}

#[test]
fn status_gives_the_note_no_command_has_printed() {
    let site = Site::new();
    let status = || site.run("so", &["update", "--status"], &[]).ok();
    assert!(status().get("pending").is_none());

    let notes = [
        (
            "updated 0.0.1 9.9.9\n",
            json!({"kind": "updated", "from": "0.0.1", "to": "9.9.9"}),
        ),
        (
            "available 9.9.9\n",
            json!({"kind": "available", "to": "9.9.9"}),
        ),
        (
            "failed 9.9.9 no macOS build\n",
            json!({"kind": "failed", "to": "9.9.9"}),
        ),
    ];
    for (line, pending) in notes {
        site.install.write(NOTE_FILE, line);
        assert_eq!(status()["pending"], pending, "{line:?}");
        // Read, not printed: the note stays for the next command at a terminal.
        assert_eq!(site.install.read(NOTE_FILE).as_deref(), Some(line));
    }
    site.install.write(NOTE_FILE, "nothing this build knows\n");
    assert!(status().get("pending").is_none());

    // One form at a time.
    for line in [
        &["update", "--check", "--status"][..],
        &["update", "--auto", "on", "--background"],
        &["update", "--auto", "maybe"],
        &["update", "--auto"],
        &["update", "now"],
    ] {
        let refused = site.run("so", line, &[]).err("E_INVALID_ARG");
        assert_eq!(refused["exit"], 2, "{line:?}");
    }
    assert_eq!(site.curl.calls(), []);
}

#[test]
fn a_marked_release_is_installed_in_the_background() {
    if !automatic_update_can_be_on() {
        return;
    }
    let site = Site::new();
    site.publish(REAL);
    let before = site.install.identity();
    let env = [(VERSION_HOOK, OLD), (AUTO_UPDATE, "on")];

    site.latest_is(REAL);
    site.mark_is(true);
    let done = site.run("so", &["update", "--background"], &env).ok();
    assert_eq!(
        done,
        json!({
            "current": REAL,
            "previous": OLD,
            "latest": REAL,
            "updated": true,
            "auto_update": "on",
            "dir": site.install.dir_text(),
            "notes": format!("{RELEASES}/tag/v{REAL}"),
            "pending": {"kind": "updated", "from": OLD, "to": REAL},
        })
    );
    let after = site.install.identity();
    assert_ne!((after[0].0, after[1].0), (before[0].0, before[1].0));
    let note = format!("updated {OLD} {REAL}\n");
    assert_eq!(site.install.read(NOTE_FILE).as_deref(), Some(note.as_str()));
    assert_eq!(
        site.install.names(),
        [RECEIPT_FILE, LOCK_FILE, NOTE_FILE, "so", "swarmops"]
    );

    // The mark asked for is the one of the line that was installed, in the release found.
    let asked = site.curl.calls();
    assert_eq!(asked.len(), 2);
    let mark = format!("url = \"{RELEASES}/download/v{REAL}/auto-update-from-0.0\"\n");
    assert!(asked[1].stdin.contains(&mark), "{}", asked[1].stdin);
    assert!(
        asked[1].stdin.contains("head\n"),
        "a HEAD: {}",
        asked[1].stdin
    );

    // The note waits for a command at a terminal: --status shows it, and the next run in the
    // background, which finds the install current, leaves it.
    let status = site.run("so", &["update", "--status"], &[]).ok();
    assert_eq!(status["pending"]["kind"], "updated");
    site.latest_is(REAL);
    let current = site
        .run("so", &["update", "--background"], &[(AUTO_UPDATE, "on")])
        .ok();
    assert_eq!(current["updated"], false);
    assert_eq!(
        site.curl.calls().len(),
        3,
        "no mark is asked for a release installed"
    );
    assert_eq!(site.install.read(NOTE_FILE).as_deref(), Some(note.as_str()));
    assert_eq!(site.install.identity(), after);
}

#[test]
fn an_unmarked_release_is_announced_at_its_second_sighting_once() {
    if !automatic_update_can_be_on() {
        return;
    }
    let site = Site::new();
    let before = site.install.identity();
    let env = [(VERSION_HOOK, OLD), (AUTO_UPDATE, "on")];
    let look = || site.run("so", &["update", "--background"], &env).ok();
    let state = || site.install.read(LOCK_FILE).unwrap_or_default();

    // 1. A mark request that got no answer says nothing of the release: no note, nothing
    //    remembered, whether the host failed or curl did.
    site.latest_is(REAL);
    site.curl.print("\n500");
    assert!(look().get("pending").is_none());
    site.latest_is(REAL);
    site.curl.fail(28);
    assert!(look().get("pending").is_none());
    site.latest_is(REAL);
    site.curl.print("https://release-assets.example/mark\n301");
    assert!(look().get("pending").is_none());
    assert_eq!(state(), "");
    assert!(!site.install.file(NOTE_FILE).exists());

    // 2. First seen without its mark: remembered, not announced. A release is the latest
    //    before its marks are uploaded.
    site.latest_is(REAL);
    site.mark_is(false);
    let first = look();
    assert_eq!(
        (&first["updated"], &first["latest"]),
        (&json!(false), &json!(REAL))
    );
    assert!(first.get("pending").is_none(), "{first}");
    assert_eq!(state(), format!("seen {REAL}\n"));
    assert!(!site.install.file(NOTE_FILE).exists());

    // 3. Still unmarked at the next look: announced.
    site.latest_is(REAL);
    site.mark_is(false);
    let second = look();
    assert_eq!(second["pending"], json!({"kind": "available", "to": REAL}));
    let note = format!("available {REAL}\n");
    assert_eq!(site.install.read(NOTE_FILE).as_deref(), Some(note.as_str()));
    assert_eq!(state(), format!("seen {REAL}\nannounced {REAL}\n"));

    // 4. And never again, once a command has printed the note.
    fs::remove_file(site.install.file(NOTE_FILE)).expect("the note was printed");
    site.latest_is(REAL);
    site.mark_is(false);
    assert!(look().get("pending").is_none());
    assert!(!site.install.file(NOTE_FILE).exists());

    assert_eq!(site.install.identity(), before);
    assert_eq!(site.release.downloads(), []);
}

#[test]
fn a_release_that_fails_in_the_background_is_not_tried_again() {
    if !automatic_update_can_be_on() {
        return;
    }
    let site = Site::new();
    site.publish(REAL);
    site.release.mismatch(REAL);
    let before = site.install.identity();
    let env = [(VERSION_HOOK, OLD), (AUTO_UPDATE, "on")];

    // 1. The marked release does not pass its check: remembered, and said once.
    site.latest_is(REAL);
    site.mark_is(true);
    let failed = site.run("so", &["update", "--background"], &env).ok();
    assert_eq!(failed["updated"], false);
    assert_eq!(failed["current"], OLD);
    assert_eq!(failed["pending"], json!({"kind": "failed", "to": REAL}));
    let note = site.install.read(NOTE_FILE).expect("a note");
    assert!(note.starts_with(&format!("failed {REAL} ")), "{note}");
    assert!(
        note.contains("does not match its line in SHA256SUMS"),
        "{note}"
    );
    assert_eq!(
        site.install.read(LOCK_FILE).as_deref(),
        Some(format!("failed {REAL}\n").as_str())
    );
    assert_eq!(site.install.identity(), before);
    let tried = site.release.downloads().len();

    // 2. The next look finds the same release and leaves it: no mark asked, nothing downloaded.
    fs::remove_file(site.install.file(NOTE_FILE)).expect("the note was printed");
    site.latest_is(REAL);
    let again = site.run("so", &["update", "--background"], &env).ok();
    assert_eq!(again["updated"], false);
    assert!(again.get("pending").is_none(), "{again}");
    assert_eq!(
        site.curl.calls().len(),
        3,
        "the latest release alone was asked for"
    );
    assert_eq!(site.release.downloads().len(), tried);
    assert!(!site.install.file(NOTE_FILE).exists());

    // 3. An update the user runs tries it, and forgets the failure when it has gone well.
    site.publish(REAL);
    site.latest_is(REAL);
    let updated = site.run("so", &["update"], &env).ok();
    assert_eq!(updated["updated"], true);
    assert_eq!(site.install.read(LOCK_FILE).as_deref(), Some(""));
    assert_ne!(site.install.identity()[0].0, before[0].0);
}

#[test]
fn background_does_nothing_where_automatic_update_is_off() {
    let site = Site::new();
    site.publish(REAL);
    let before = site.install.identity();
    let background = |env: &[(&str, &str)]| {
        let env = [&[(VERSION_HOOK, OLD)][..], env].concat();
        site.run("so", &["update", "--background"], &env).ok()
    };
    // The version is the running program's own, by the hook: nothing looked any further.
    let nothing = |result: &Value, auto: &str| {
        let expected = json!({
            "current": OLD,
            "updated": false,
            "auto_update": auto,
            "dir": site.install.dir_text(),
        });
        assert_eq!(*result, expected, "{auto}");
    };

    // 1. Each thing that turns it off, alone: the harness's variable, a value that is no
    //    switch, CI, the receipt, and a build that is no release's.
    nothing(&background(&[]), "off_env");
    nothing(&background(&[(AUTO_UPDATE, "off")]), "off_env");
    nothing(&background(&[(AUTO_UPDATE, "OFF")]), "off_env");
    nothing(&background(&[(AUTO_UPDATE, ""), ("CI", "1")]), "off_ci");
    nothing(
        &background(&[(AUTO_UPDATE, "on"), (RELEASE_HOOK, "")]),
        "off_source_build",
    );
    site.install.write(RECEIPT_FILE, RECEIPT_OFF);
    nothing(&background(&[(AUTO_UPDATE, "on")]), "off");
    site.install.write(RECEIPT_FILE, RECEIPT_ON);
    assert_eq!(
        site.install.names(),
        [RECEIPT_FILE, "so", "swarmops"],
        "no lock was made"
    );

    // 2. On, with another update holding the lock: it leaves without a request.
    let lock = File::create(site.install.file(LOCK_FILE)).expect("make the lock file");
    lock.lock().expect("hold the lock");
    nothing(&background(&[(AUTO_UPDATE, "on")]), on_for_this_user());
    drop(lock);

    // 3. On, with no curl to ask with, or a host that does not answer: silent, exit 0.
    let nowhere = [
        (AUTO_UPDATE, "on"),
        (CURL_ENV, "/nonexistent/swarmops-test/curl"),
    ];
    nothing(&background(&nowhere), on_for_this_user());
    assert_eq!(site.curl.calls(), []);
    if automatic_update_can_be_on() {
        site.curl.fail(7);
        nothing(&background(&[(AUTO_UPDATE, "on")]), "on");
        assert_eq!(site.curl.calls().len(), 1);
    }

    assert_eq!(site.release.downloads(), []);
    assert!(!site.install.file(NOTE_FILE).exists());
    assert_eq!(site.install.identity(), before);
}

/// The test kit spells the names of the files beside the binaries itself, since it names no
/// crate above the repository: they are the program's.
#[test]
fn the_test_kit_s_file_names_are_the_program_s() {
    use swarmops_ops::update;

    assert_eq!(RECEIPT_FILE, update::RECEIPT_FILE);
    assert_eq!(LOCK_FILE, update::LOCK_FILE);
    assert_eq!(NOTE_FILE, update::NOTE_FILE);
    assert_eq!(STAGING_DIR, update::STAGING_DIR);
    for (text, on) in [(RECEIPT_ON, true), (RECEIPT_OFF, false)] {
        let receipt = update::Receipt::parse(text).expect("a receipt");
        assert_eq!(receipt.auto_update(), on);
    }
    assert_eq!(RELEASES, update::RELEASES_URL);
    assert_eq!(AUTO_UPDATE, update::AUTO_UPDATE_ENV);
}
