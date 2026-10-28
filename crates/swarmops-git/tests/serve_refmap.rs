//! The ref map of `swarmops serve` (`swarmops_git::serve::refmap`, M14 design §3): each row of
//! the design's table in both directions and both formats, and the store names git refuses,
//! checked against `git check-ref-format` itself. Written by the M14 plan's task S1.

use std::process::Command;

use swarmops_git::serve::refmap::{Mapped, Refusal, from_git, to_git};
use swarmops_store::{RefKind, StoreFormat, ref_kind};
use swarmops_testkit::git::GIT_ENV;

/// How many names the grammar test generates.
const GENERATED: usize = 500;

/// The suffix git refuses a ref name component to end in: its lock files, case and all.
const LOCK_SUFFIX: &str = ".lock";

/// The seed of the generator: a fixed one, so a failure names the same names again.
const SEED: u64 = 0x5e7e_f00d_0000_0014;

/// The bytes a generated segment is made of: the store grammar's, weighted toward `.` and `-`,
/// where git's rules and the store's part.
const ALPHABET: &[u8] = b"aZ09._-..--k";

/// The store names git takes as served refs: (kind, store name, git ref).
const SERVED: &[(RefKind, &str, &str)] = &[
    (RefKind::Main, "main", "refs/heads/main"),
    (RefKind::Release, "release/1", "refs/heads/release/1"),
    (
        RefKind::Release,
        "release/1.0-rc_2",
        "refs/heads/release/1.0-rc_2",
    ),
    (RefKind::Branch, "feature/x", "refs/heads/feature/x"),
    (RefKind::Branch, "a", "refs/heads/a"),
    (RefKind::Branch, "Main", "refs/heads/Main"),
    (RefKind::Tag, "tags/v1", "refs/tags/v1"),
    (RefKind::Tag, "tags/v1.4.0", "refs/tags/v1.4.0"),
    (RefKind::Tag, "tags/team/v2", "refs/tags/team/v2"),
];

/// The FORMAT 1 names among [`SERVED`]: the rest need FORMAT 2.
const FORMAT_1: &[&str] = &["main", "release/1", "release/1.0-rc_2"];

/// Each served store ref maps to its git ref, and back, in each format that takes it; in FORMAT 1
/// a FORMAT 2 name is `FormatRequired { needs: V2 }`.
#[test]
fn served_refs_map_both_ways() {
    for &(kind, store, git) in SERVED {
        assert_eq!(to_git(kind, store).as_deref(), Some(git), "{store}");
        let mapped = Mapped::Ref {
            store: store.to_owned(),
            kind,
        };
        assert_eq!(from_git(StoreFormat::V2, git), mapped, "{git} in FORMAT 2");

        let v1 = if FORMAT_1.contains(&store) {
            mapped
        } else {
            Mapped::Refused(Refusal::FormatRequired {
                needs: StoreFormat::V2,
            })
        };
        assert_eq!(from_git(StoreFormat::V1, git), v1, "{git} in FORMAT 1");
    }
}

/// Remote-tracking refs are never served, and a name of another kind than the one given maps to
/// nothing.
#[test]
fn unserved_store_refs_have_no_git_ref() {
    let none = [
        (RefKind::RemoteTracking, "remote/origin/main"),
        (RefKind::Branch, "main"),
        (RefKind::Main, "feature/x"),
        (RefKind::Tag, "feature/x"),
        (RefKind::Branch, "tags/v1"),
        (RefKind::Branch, "HEAD"),
        (RefKind::Branch, "a..b"),
    ];
    for (kind, store) in none {
        assert_eq!(to_git(kind, store), None, "{kind:?} {store}");
    }
}

/// The git refs a client may name that stand for no store ref: what the store reserves, and what
/// `serve` does not serve, in both formats.
#[test]
fn unserved_git_refs_are_refused() {
    let refused = [
        ("HEAD", Refusal::Reserved),
        ("refs/heads/remote/origin/x", Refusal::Reserved),
        ("refs/heads/tags/v1", Refusal::Reserved),
        ("refs/heads/main/x", Refusal::Reserved),
        ("refs/heads/HEAD", Refusal::Reserved),
        ("refs/notes/x", Refusal::NotServed),
        ("refs/remotes/origin/main", Refusal::NotServed),
        ("refs/pull/1/head", Refusal::NotServed),
        ("refs/heads/a@b", Refusal::NotServed),
        ("refs/heads/a..b", Refusal::NotServed),
        ("refs/tags/.v1", Refusal::NotServed),
        ("refs/land/tags/v1", Refusal::NotServed),
        ("refs/land/remote/o/x", Refusal::NotServed),
        ("refs/heads", Refusal::NotServed),
    ];
    for format in [StoreFormat::V1, StoreFormat::V2] {
        for (git, why) in refused {
            assert_eq!(
                from_git(format, git),
                Mapped::Refused(why),
                "{git} {format:?}"
            );
        }
    }
}

/// `refs/land/<target>` names a land onto a branch the format takes.
#[test]
fn land_refs_name_their_target() {
    let land = |target: &str| Mapped::Land {
        target: target.to_owned(),
    };
    for format in [StoreFormat::V1, StoreFormat::V2] {
        assert_eq!(from_git(format, "refs/land/main"), land("main"));
        assert_eq!(from_git(format, "refs/land/release/2"), land("release/2"));
    }
    assert_eq!(
        from_git(StoreFormat::V2, "refs/land/feature/x"),
        land("feature/x")
    );
    assert_eq!(
        from_git(StoreFormat::V1, "refs/land/feature/x"),
        Mapped::Refused(Refusal::FormatRequired {
            needs: StoreFormat::V2
        })
    );
}

/// Every store name of the FORMAT 2 grammar, 500 of them generated, maps to a git ref exactly
/// when `git check-ref-format` takes that ref; the ones git refuses are `release/<x>` names
/// alone, whose `x` holds `..` or ends in `.` or `.lock`: the list below, planted in the
/// generated names so that each class is seen.
#[test]
fn store_names_git_refuses_are_listed() {
    let planted = [
        "release/a..b",
        "release/x.",
        "release/x.lock",
        "release/v1.2..3",
    ];
    let names: Vec<String> = planted
        .iter()
        .map(|name| (*name).to_owned())
        .chain(generated_names())
        .collect();

    // Each name: the map's answer against git's own.
    let mut refused = Vec::new();
    for name in &names {
        let kind = ref_kind(name).unwrap_or_else(|| panic!("{name} is in the grammar"));
        let full = match kind {
            RefKind::Tag => format!("refs/tags/{}", &name["tags/".len()..]),
            RefKind::RemoteTracking => continue,
            _ => format!("refs/heads/{name}"),
        };
        let git_takes = check_ref_format(&full);
        assert_eq!(to_git(kind, name).is_some(), git_takes, "{name} ({full})");
        if !git_takes {
            refused.push(name.clone());
        }
    }

    // What git refuses: release names whose x breaks a rule git has and FORMAT 1 has not.
    for name in &refused {
        let x = name
            .strip_prefix("release/")
            .unwrap_or_else(|| panic!("{name}: git refuses a store name that is no release name"));
        let listed = x.contains("..") || x.ends_with('.') || x.ends_with(LOCK_SUFFIX);
        assert!(listed, "{name}: git refuses it for a reason not listed");
    }
    for name in planted {
        assert!(refused.iter().any(|r| r == name), "{name} was not refused");
    }
}

/// [`GENERATED`] names the FORMAT 2 grammar takes, each from [`SEED`]'s sequence: one to four
/// segments of [`ALPHABET`], under `release/`, `tags/` or none, kept when [`ref_kind`] takes them.
fn generated_names() -> Vec<String> {
    let mut state = SEED;
    let mut next = move |below: u64| {
        // A 64-bit linear congruential step (Knuth's MMIX constants).
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) % below
    };
    let mut names = Vec::new();
    while names.len() < GENERATED {
        let prefix = ["", "release/", "tags/", "main"][usize::try_from(next(4)).unwrap()];
        let segments = 1 + next(if prefix == "release/" { 1 } else { 4 });
        let mut parts = Vec::new();
        for _ in 0..segments {
            let len = 1 + next(6);
            let segment: String = (0..len)
                .map(|_| char::from(ALPHABET[usize::try_from(next(12)).unwrap()]))
                .collect();
            parts.push(segment);
        }
        let name = format!("{prefix}{}", parts.join("/"));
        if ref_kind(&name).is_some() {
            names.push(name);
        }
    }
    names
}

/// Whether `git check-ref-format` takes `full`, by the git `SWARMOPS_GIT` names (else `git` on
/// `PATH`), without any configuration.
fn check_ref_format(full: &str) -> bool {
    let program = std::env::var_os(GIT_ENV).unwrap_or_else(|| "git".into());
    let mut command = Command::new(program);
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("LC_ALL", "C")
        .args(["check-ref-format", full]);
    let output = swarmops_testkit::child::run(&mut command);
    match output.status.code() {
        Some(0) => true,
        Some(1) => false,
        _ => panic!("git check-ref-format {full}: {output:?}"),
    }
}
