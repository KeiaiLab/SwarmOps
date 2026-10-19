//! git's commit and tree bytes, parsed and rendered: every object of the fixtures renders back to
//! its own bytes, a commit written otherwise than git writes it parses but renders otherwise, and
//! what the fields cannot hold at all is malformed.

use proptest::prelude::*;
use swarmops_git::{
    GitOid, RawCommit, RawError, RawIdent, RawKind, RawTree, RawTreeEntry, git_hash,
};
use swarmops_testkit::git;

/// The fixtures whose commits git's own bytes do not reproduce, on purpose: their scripts craft
/// commits that the parse can hold only approximately (an ident with two spaces before its `<`,
/// a date git cannot read, a header without a value, no blank line before the message), which
/// the conversion classes `nonstandard`. Every other fixture's commits render back exactly.
const NOT_REPRODUCED: [&str; 1] = ["nonstandard"];

/// The empty tree's id, for the inline commits.
const TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

fn oid(hex: &str) -> GitOid {
    hex.parse().unwrap_or_else(|error| panic!("{hex}: {error}"))
}

/// `bytes`, printable, for an assertion's message.
fn shown(bytes: &[u8]) -> String {
    bytes.escape_ascii().to_string()
}

/// The ids in their order, each once.
fn each_once(ids: &[GitOid]) -> Vec<GitOid> {
    let mut seen = Vec::new();
    for id in ids {
        if !seen.contains(id) {
            seen.push(*id);
        }
    }
    seen
}

/// A commit from its lines, each ended by a newline, then a blank line and `message`.
fn commit(headers: &[&[u8]], message: &[u8]) -> Vec<u8> {
    let mut raw = Vec::new();
    for header in headers {
        raw.extend_from_slice(header);
        raw.push(b'\n');
    }
    raw.push(b'\n');
    raw.extend_from_slice(message);
    raw
}

fn tree_line() -> Vec<u8> {
    format!("tree {TREE}").into_bytes()
}

#[test]
fn every_fixture_commit_and_tree_renders_back_to_its_bytes() {
    let (mut commits, mut trees) = (0, 0);
    for name in git::names() {
        let fixture = git::fixture(&name);
        let Some(manifest) = fixture.manifest else {
            continue;
        };
        // A SHA-256 repository's ids are not SHA-1s; the source refuses it before any read.
        if manifest.object_format != "sha1" {
            continue;
        }
        for commit in &manifest.commits {
            let id = oid(&commit.oid);
            assert_eq!(
                git_hash(RawKind::Commit, &commit.raw),
                id,
                "{name}: commit {id}"
            );
            let parsed = RawCommit::parse(&commit.raw)
                .unwrap_or_else(|error| panic!("{name}: commit {id}: {error}"));
            assert_eq!(parsed.tree, oid(&commit.tree), "{name}: commit {id}'s tree");
            // git lists no parent for a shallow clone's oldest commits, whose bytes name theirs:
            // its `shallow` file grafts them away.
            if !manifest.shallow {
                let parents: Vec<GitOid> = commit.parents.iter().map(|p| oid(p)).collect();
                assert_eq!(
                    each_once(&parsed.parents),
                    each_once(&parents),
                    "{name}: commit {id}'s parents"
                );
            }
            if !NOT_REPRODUCED.contains(&name.as_str()) {
                let rendered = parsed.render();
                assert!(
                    rendered == commit.raw,
                    "{name}: commit {id} renders as\n{}\nnot as\n{}",
                    shown(&rendered),
                    shown(&commit.raw)
                );
            }
            commits += 1;
        }
        for (hex, bytes) in &manifest.trees {
            let id = oid(hex);
            assert_eq!(git_hash(RawKind::Tree, bytes), id, "{name}: tree {id}");
            let parsed =
                RawTree::parse(bytes).unwrap_or_else(|error| panic!("{name}: tree {id}: {error}"));
            assert!(
                parsed.render() == *bytes,
                "{name}: tree {id} renders otherwise"
            );
            trees += 1;
        }
    }
    assert!(
        commits > 0 && trees > 0,
        "no fixture commit or tree was read"
    );
}

#[test]
fn a_continuation_line_and_an_encoding_round_trip() {
    let raw = commit(
        &[
            &tree_line(),
            b"author A <a@x> 1700000000 +0000",
            b"committer C <c@x> 1700000060 +0100",
            b"encoding ISO-8859-1",
            b"gpgsig -----BEGIN PGP SIGNATURE-----",
            b" ",
            b" iQEzBAABCAAdFiEE",
            b" -----END PGP SIGNATURE-----",
        ],
        b"caf\xe9\n",
    );
    let parsed = RawCommit::parse(&raw).unwrap();
    assert_eq!(
        parsed.extra_headers,
        vec![
            (b"encoding".to_vec(), b"ISO-8859-1".to_vec()),
            (
                b"gpgsig".to_vec(),
                b"-----BEGIN PGP SIGNATURE-----\n\niQEzBAABCAAdFiEE\n-----END PGP SIGNATURE-----"
                    .to_vec()
            ),
        ]
    );
    assert_eq!(parsed.message, b"caf\xe9\n");
    assert_eq!(parsed.committer.offset_secs, 3600);
    assert_eq!(parsed.render(), raw);
}

#[test]
fn negative_zero_and_a_negative_time_round_trip() {
    // (the ident's date, its seconds, its offset in seconds, whether it is git's `-0000`)
    let dates: [(&str, i64, i32, bool); 6] = [
        ("-1 -0000", -1, 0, true),
        ("-86400 +0000", -86_400, 0, false),
        ("0 +1400", 0, 50_400, false),
        ("1700000000 -1200", 1_700_000_000, -43_200, false),
        ("1700000000 +0530", 1_700_000_000, 19_800, false),
        ("-9223372036854775808 -0001", i64::MIN, -60, false),
    ];
    for (date, secs, offset_secs, negative_zero) in dates {
        let author = format!("author A <a@x> {date}");
        let raw = commit(
            &[
                &tree_line(),
                author.as_bytes(),
                b"committer C <c@x> 1 +0000",
            ],
            b"m\n",
        );
        let parsed = RawCommit::parse(&raw).unwrap();
        let expected = RawIdent {
            name: b"A".to_vec(),
            email: b"a@x".to_vec(),
            secs,
            offset_secs,
            negative_zero,
        };
        assert_eq!(parsed.author, expected, "{date}");
        assert_eq!(parsed.render(), raw, "{date}");
    }
}

#[test]
fn an_ident_it_cannot_reproduce_parses_but_renders_otherwise() {
    // (the author line, the ident it parses to, the author line it renders)
    let cases: [(&[u8], RawIdent, &[u8]); 4] = [
        (
            b"author A  <a@x> 1 +0000",
            ident(b"A", b"a@x", 1, 0),
            b"author A <a@x> 1 +0000",
        ),
        (
            b"author A <a@x> 1 +05:30",
            ident(b"A", b"a@x", 0, 0),
            b"author A <a@x> 0 +0000",
        ),
        (
            b"author A <a@x>",
            ident(b"A", b"a@x", 0, 0),
            b"author A <a@x> 0 +0000",
        ),
        (
            b"author nobody",
            ident(b"nobody", b"", 0, 0),
            b"author nobody <> 0 +0000",
        ),
    ];
    for (line, expected, rendered_line) in cases {
        let raw = commit(&[&tree_line(), line, b"committer C <c@x> 1 +0000"], b"m\n");
        let parsed =
            RawCommit::parse(&raw).unwrap_or_else(|error| panic!("{}: {error}", shown(line)));
        assert_eq!(parsed.author, expected, "{}", shown(line));
        let expected_raw = commit(
            &[&tree_line(), rendered_line, b"committer C <c@x> 1 +0000"],
            b"m\n",
        );
        assert_eq!(parsed.render(), expected_raw, "{}", shown(line));
        assert_ne!(parsed.render(), raw, "{}", shown(line));
    }
}

fn ident(name: &[u8], email: &[u8], secs: i64, offset_secs: i32) -> RawIdent {
    RawIdent {
        name: name.to_vec(),
        email: email.to_vec(),
        secs,
        offset_secs,
        negative_zero: false,
    }
}

#[test]
fn a_header_without_a_space_and_a_missing_blank_line_parse_but_render_otherwise() {
    let author: &[u8] = b"author A <a@x> 1 +0000";
    let committer: &[u8] = b"committer C <c@x> 1 +0000";

    // A header line without a space: its key alone, an empty value, rendered with the space.
    let raw = commit(&[&tree_line(), author, committer, b"x-flag"], b"m\n");
    let parsed = RawCommit::parse(&raw).unwrap();
    assert_eq!(parsed.extra_headers, vec![(b"x-flag".to_vec(), Vec::new())]);
    assert_eq!(
        parsed.render(),
        commit(&[&tree_line(), author, committer, b"x-flag "], b"m\n")
    );

    // No blank line: every line a header, an empty message, rendered with the blank line.
    let mut raw = [
        tree_line().as_slice(),
        b"\n",
        author,
        b"\n",
        committer,
        b"\nx-key v\n",
    ]
    .concat();
    let parsed = RawCommit::parse(&raw).unwrap();
    assert_eq!(
        parsed.extra_headers,
        vec![(b"x-key".to_vec(), b"v".to_vec())]
    );
    assert_eq!(parsed.message, b"");
    raw.push(b'\n');
    assert_eq!(parsed.render(), raw);
}

#[test]
fn a_commit_it_cannot_hold_is_malformed() {
    let parent = format!("parent {TREE}");
    let author: &[u8] = b"author A <a@x> 1 +0000";
    let committer: &[u8] = b"committer C <c@x> 1 +0000";
    let cases: [(&str, Vec<u8>); 9] = [
        ("no tree", commit(&[author, committer], b"m\n")),
        (
            "the tree not first",
            commit(
                &[parent.as_bytes(), &tree_line(), author, committer],
                b"m\n",
            ),
        ),
        (
            "a parent after the author",
            commit(
                &[&tree_line(), author, parent.as_bytes(), committer],
                b"m\n",
            ),
        ),
        (
            "a parent after the committer",
            commit(
                &[&tree_line(), author, committer, parent.as_bytes()],
                b"m\n",
            ),
        ),
        (
            "a 39-digit tree id",
            commit(&[&tree_line()[..44], author, committer], b"m\n"),
        ),
        (
            "a tree id that is not hex",
            commit(
                &[
                    b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee490g",
                    author,
                    committer,
                ],
                b"m\n",
            ),
        ),
        ("no author", commit(&[&tree_line(), committer], b"m\n")),
        ("no committer", commit(&[&tree_line(), author], b"m\n")),
        ("nothing", Vec::new()),
    ];
    for (case, raw) in cases {
        assert!(
            matches!(RawCommit::parse(&raw), Err(RawError::Malformed { .. })),
            "{case}: {:?}",
            RawCommit::parse(&raw)
        );
    }
}

#[test]
fn tree_modes_and_order_are_kept_as_stored() {
    let (b, a) = ([0xbb; 20], [0xaa; 20]);
    let raw = [&b"100664 b\0"[..], &b, b"040000 a\0", &a].concat();
    let parsed = RawTree::parse(&raw).unwrap();
    assert_eq!(
        parsed.entries,
        vec![
            RawTreeEntry {
                mode: b"100664".to_vec(),
                name: b"b".to_vec(),
                oid: GitOid(b),
            },
            RawTreeEntry {
                mode: b"040000".to_vec(),
                name: b"a".to_vec(),
                oid: GitOid(a),
            },
        ]
    );
    assert_eq!(parsed.render(), raw);
}

#[test]
fn a_tree_it_cannot_read_is_malformed() {
    let id = [0x11; 20];
    let cases: [(&str, Vec<u8>); 5] = [
        ("no NUL after the name", b"100644 a".to_vec()),
        ("an id cut short", [&b"100644 a\0"[..], &id[..19]].concat()),
        (
            "a mode that is not octal",
            [&b"100844 a\0"[..], &id].concat(),
        ),
        ("an empty mode", [&b" a\0"[..], &id].concat()),
        ("no space after the mode", [&b"100644\0"[..], &id].concat()),
    ];
    for (case, raw) in cases {
        assert!(
            matches!(RawTree::parse(&raw), Err(RawError::Malformed { .. })),
            "{case}: {:?}",
            RawTree::parse(&raw)
        );
    }
}

/// An ident whose parse gives it back: a name or e-mail of any bytes but `<`, `>` and a newline,
/// the name not ending in the white space git's ident parse trims; any time; an offset of whole
/// minutes up to `±9959`, and `-0000` only at offset zero.
fn arb_ident() -> impl Strategy<Value = RawIdent> {
    let text = || {
        proptest::collection::vec(any::<u8>(), 0..12).prop_map(|bytes| {
            bytes
                .into_iter()
                .filter(|b| !b"<>\n".contains(b))
                .collect::<Vec<u8>>()
        })
    };
    (
        text().prop_filter("a name git's parse keeps", |name: &Vec<u8>| {
            !name.last().is_some_and(|b| b" \t\r".contains(b))
        }),
        text(),
        any::<i64>(),
        (any::<bool>(), 0..100_i32, 0..60_i32),
        any::<bool>(),
    )
        .prop_map(
            |(name, email, secs, (west, hours, minutes), negative_zero)| {
                let offset_secs = (hours * 3600 + minutes * 60) * if west { -1 } else { 1 };
                RawIdent {
                    name,
                    email,
                    secs,
                    offset_secs,
                    negative_zero: negative_zero && offset_secs == 0,
                }
            },
        )
}

/// A header the parse gives back: a key of lowercase letters, digits and dashes that is not
/// `parent`, and a value of any bytes, newlines included (continuation lines).
fn arb_header() -> impl Strategy<Value = (Vec<u8>, Vec<u8>)> {
    (
        "[a-z][a-z0-9-]{0,10}".prop_filter("not a parent", |key| key != "parent"),
        proptest::collection::vec(any::<u8>(), 0..40),
    )
        .prop_map(|(key, value)| (key.into_bytes(), value))
}

fn arb_oid() -> impl Strategy<Value = GitOid> {
    any::<[u8; 20]>().prop_map(GitOid)
}

proptest! {
    #[test]
    fn rendered_commits_parse_back_unchanged(
        tree in arb_oid(),
        parents in proptest::collection::vec(arb_oid(), 0..4),
        author in arb_ident(),
        committer in arb_ident(),
        extra_headers in proptest::collection::vec(arb_header(), 0..5),
        message in proptest::collection::vec(any::<u8>(), 0..60),
    ) {
        let commit = RawCommit { tree, parents, author, committer, extra_headers, message };
        prop_assert_eq!(RawCommit::parse(&commit.render()), Ok(commit));
    }

    #[test]
    fn rendered_trees_parse_back_unchanged(
        entries in proptest::collection::vec(
            ("[0-7]{1,7}", proptest::collection::vec(any::<u8>(), 0..12), arb_oid()),
            0..6,
        ),
    ) {
        let tree = RawTree {
            entries: entries
                .into_iter()
                .map(|(mode, name, oid)| RawTreeEntry {
                    mode: mode.into_bytes(),
                    name: name.into_iter().filter(|&b| b != 0).collect(),
                    oid,
                })
                .collect(),
        };
        prop_assert_eq!(RawTree::parse(&tree.render()), Ok(tree));
    }

    #[test]
    fn any_bytes_that_parse_as_a_tree_render_back_and_no_bytes_panic(
        bytes in proptest::collection::vec(any::<u8>(), 0..200),
    ) {
        if let Ok(tree) = RawTree::parse(&bytes) {
            prop_assert_eq!(tree.render(), bytes.clone());
        }
        let _ = RawCommit::parse(&bytes);
    }
}
