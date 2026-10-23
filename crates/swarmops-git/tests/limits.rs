//! GitHub's documented limits (M13): which hosts they apply to, and the size arithmetic of a
//! push's files and its whole. Written by the M13 plan's task C7.

use swarmops_git::GitOid;
use swarmops_git::limits::{
    GITHUB_FILE_REFUSED, GITHUB_FILE_WARNED, GITHUB_PUSH_REFUSED, LimitExceeded, LimitScope,
    applies_to, check,
};
use swarmops_git::transport::NewObjects;

/// A push's new objects: one blob per `(size, path)`, and `disk_usage` bytes on disk.
fn objects(blobs: &[(u64, &str)], disk_usage: u64) -> NewObjects {
    let blobs = blobs
        .iter()
        .enumerate()
        .map(|(n, (size, path))| {
            let byte = u8::try_from(n).expect("a few blobs");
            (GitOid::new([byte; 20]), *size, (*path).to_owned())
        })
        .collect();
    let mut objects = NewObjects::default();
    objects.blobs = blobs;
    objects.disk_usage = disk_usage;
    objects
}

/// The values are GitHub's, as its pages give them: 100 MiB and 50 MiB for a file, binary, and
/// 2 GB for a push, read as the smaller 2 × 10^9 bytes.
#[test]
fn the_limits_are_githubs() {
    assert_eq!(GITHUB_FILE_REFUSED, 104_857_600);
    assert_eq!(GITHUB_FILE_WARNED, 52_428_800);
    assert_eq!(GITHUB_PUSH_REFUSED, 2_000_000_000);
}

/// The limits apply to `github.com` and its subdomains, in any case, and to no other host: not
/// a name that merely ends in `github.com`, nor one that holds it.
#[test]
fn limits_apply_to_github_hosts_only() {
    for host in [
        "github.com",
        "ssh.github.com",
        "GitHub.com",
        "a.b.github.com",
    ] {
        assert!(applies_to(host), "{host}");
    }
    for host in [
        "notgithub.com",
        "github.com.example.org",
        "gitlab.com",
        "127.0.0.1",
        "",
        ".github.com",
    ] {
        assert!(!applies_to(host), "{host:?}");
    }
}

/// A push of small files passes with no warning.
#[test]
fn small_files_pass_silently() {
    let found = check(&objects(&[(10, "a.txt"), (1 << 20, "b.bin")], 2 << 20));
    assert_eq!(found, Ok(Vec::new()));
}

/// A file of exactly 50 MiB passes silently; one byte more is warned about, by its path; the
/// push still passes.
#[test]
fn limits_warn_from_50_mib() {
    let at = check(&objects(&[(GITHUB_FILE_WARNED, "at.bin")], 1 << 20));
    assert_eq!(at, Ok(Vec::new()));

    let over = check(&objects(
        &[(GITHUB_FILE_WARNED + 1, "big/over.bin")],
        1 << 20,
    ));
    let warnings = over.expect("a warning, not a refusal");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("big/over.bin"), "{warnings:?}");
}

/// A file of exactly 100 MiB is warned about; one byte more refuses the push, naming the file,
/// its size and the limit.
#[test]
fn limits_refuse_a_large_blob_with_its_path() {
    let at = check(&objects(&[(GITHUB_FILE_REFUSED, "at.bin")], 1 << 20));
    assert_eq!(at.map(|warnings| warnings.len()), Ok(1));

    let over = objects(
        &[(1, "small.txt"), (GITHUB_FILE_REFUSED + 1, "data/huge.bin")],
        1 << 20,
    );
    let refused = LimitExceeded::new(
        LimitScope::RemoteFile,
        Some("data/huge.bin".to_owned()),
        GITHUB_FILE_REFUSED + 1,
        GITHUB_FILE_REFUSED,
    );
    assert_eq!(check(&over), Err(refused));
}

/// A push of exactly 2 × 10^9 bytes on disk passes; one byte more is refused as a whole, with
/// no path.
#[test]
fn limits_refuse_a_push_over_2_gb() {
    let at = check(&objects(&[(1, "a.txt")], GITHUB_PUSH_REFUSED));
    assert_eq!(at, Ok(Vec::new()));

    let over = check(&objects(&[(1, "a.txt")], GITHUB_PUSH_REFUSED + 1));
    let refused = LimitExceeded::new(
        LimitScope::RemotePush,
        None,
        GITHUB_PUSH_REFUSED + 1,
        GITHUB_PUSH_REFUSED,
    );
    assert_eq!(over, Err(refused));
}

/// A file over the limit is named even when the push is also over its own: the file is what
/// the user can act on.
#[test]
fn a_large_file_is_named_before_the_push_size() {
    let both = objects(
        &[(GITHUB_FILE_REFUSED + 1, "x.bin")],
        GITHUB_PUSH_REFUSED + 1,
    );
    let refused = check(&both).expect_err("refused");
    assert_eq!(refused.scope, LimitScope::RemoteFile);
    assert_eq!(refused.path.as_deref(), Some("x.bin"));
}
