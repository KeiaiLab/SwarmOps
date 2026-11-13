//! The repository the import reads, through gix: every object of the fixtures read by its id, refs
//! resolved to commits as git resolves them, replacement refs never followed, and the repositories
//! it does not read refused when it opens them. And through `git cat-file` ([`CatFileSource`]):
//! the same objects as gix reads, a quarantine's first, a damaged one `Corrupt`.

use std::fs;
use std::path::Path;

use swarmops_git::{
    CatFileSource, GitOid, GitSource, GixSource, ObjectFormat, RawKind, SourceError, Unsupported,
};
use swarmops_testkit::git::{self, GitFixture, Manifest};

use crate::support::MemSource;

fn oid(hex: &str) -> GitOid {
    hex.parse().unwrap_or_else(|error| panic!("{hex}: {error}"))
}

fn repo(fixture: &GitFixture) -> &Path {
    fixture
        .repo
        .as_deref()
        .expect("the fixture holds a repository")
}

fn manifest(fixture: &GitFixture) -> &Manifest {
    fixture
        .manifest
        .as_ref()
        .expect("the fixture has a manifest")
}

fn open(fixture: &GitFixture) -> GixSource {
    GixSource::open(repo(fixture)).unwrap_or_else(|error| panic!("open: {error}"))
}

#[test]
fn gix_source_reads_every_fixture_object_by_its_oid() {
    let mut read = 0;
    for name in git::names() {
        let fixture = git::fixture(&name);
        let (Some(repo), Some(manifest)) = (&fixture.repo, &fixture.manifest) else {
            continue;
        };
        // What the source refuses when it opens it (`gix_source_refuses_what_it_does_not_read`,
        // `a_reftable_repository_is_unsupported`): another object format, a shallow or a partial
        // clone, and refs git keeps in a reftable (its `reftable/` directory), which only a git
        // of 2.45 or newer makes.
        if !readable(&name, manifest, repo) {
            continue;
        }
        let source = GixSource::open(repo).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(source.object_format(), ObjectFormat::Sha1, "{name}");
        for commit in &manifest.commits {
            let id = oid(&commit.oid);
            let got = source
                .read(id)
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            assert!(
                got == (RawKind::Commit, commit.raw.clone()),
                "{name}: commit {id}"
            );
            let size = u64::try_from(commit.raw.len()).unwrap();
            assert_eq!(
                source.header(id),
                Ok((RawKind::Commit, size)),
                "{name}: {id}"
            );
            read += 1;
        }
        for (hex, bytes) in &manifest.trees {
            let got = source
                .read(oid(hex))
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            assert!(got == (RawKind::Tree, bytes.clone()), "{name}: tree {hex}");
            read += 1;
        }
        for (hex, size) in &manifest.blobs {
            assert_eq!(
                source.header(oid(hex)),
                Ok((RawKind::Blob, *size)),
                "{name}: blob {hex}"
            );
            read += 1;
        }
    }
    assert!(read > 0, "no fixture object was read");
}

#[test]
fn resolve_peels_head_names_tags_and_ids() {
    let fixture = git::fixture("merges");
    let manifest = manifest(&fixture);
    let source = open(&fixture);
    let main = oid(&manifest.refs["refs/heads/main"]);
    for name in ["HEAD", "main", "refs/heads/main", "heads/main"] {
        assert_eq!(source.resolve(name), Ok(main), "{name}");
    }
    // An annotated tag peels to its commit, a lightweight one names it.
    assert_eq!(
        source.resolve("v1"),
        Ok(oid(&manifest.refs["refs/tags/v1"]))
    );
    assert_eq!(
        source.resolve("refs/tags/v1"),
        Ok(oid(&manifest.refs["refs/tags/v1"]))
    );
    assert_eq!(
        source.resolve("light"),
        Ok(oid(&manifest.refs["refs/tags/light"]))
    );
    // 40 hex digits of either case name the object itself.
    let first = &manifest.commits[0];
    assert_eq!(source.resolve(&first.oid), Ok(oid(&first.oid)));
    assert_eq!(
        source.resolve(&first.oid.to_uppercase()),
        Ok(oid(&first.oid))
    );
    // A tree is no commit.
    assert_eq!(
        source.resolve(&first.tree),
        Err(SourceError::NotACommit {
            name: first.tree.clone(),
            kind: RawKind::Tree,
        })
    );
    // No ref, no object of that id, and no revision syntax (gix's `revision` feature is off).
    for name in [
        "nope",
        "main~3",
        "a..b",
        "0123456789abcdef0123456789abcdef01234567",
    ] {
        assert_eq!(
            source.resolve(name),
            Err(SourceError::RefNotFound {
                name: name.to_owned()
            }),
            "{name}"
        );
    }
    // A `HEAD` that names a branch with no commit yet.
    fs::write(repo(&fixture).join("HEAD"), "ref: refs/heads/unborn\n").unwrap();
    assert_eq!(
        open(&fixture).resolve("HEAD"),
        Err(SourceError::RefNotFound {
            name: "HEAD".to_owned()
        })
    );
}

/// An annotated tag whose header says it is longer than an object holds is refused as `resolve`
/// peels it, before it is read: its loose file, a hundred bytes or so, claims more than 512 MiB
/// once inflated.
#[test]
fn resolve_refuses_a_tag_past_the_object_limit_before_it_reads_it() {
    use swarmops_store::MAX_OBJECT_LEN;

    let repository = git::empty_repository();
    let huge = "5a".repeat(20);
    let mut file = format!("tag {}\0", MAX_OBJECT_LEN + 1).into_bytes();
    file.extend_from_slice(
        format!("object {}\ntype commit\ntag huge\n", "11".repeat(20)).as_bytes(),
    );
    git::write_loose_object(&repository.dir, &huge, &file);
    fs::write(repository.dir.join("refs/tags/huge"), format!("{huge}\n")).unwrap();
    let source = GixSource::open(&repository.dir).unwrap_or_else(|error| panic!("open: {error}"));
    assert_eq!(
        source.header(oid(&huge)),
        Ok((RawKind::Tag, MAX_OBJECT_LEN + 1))
    );
    let refused = SourceError::TooLarge {
        oid: oid(&huge),
        kind: RawKind::Tag,
        size: 536_870_913,
    };
    for name in ["huge", "refs/tags/huge", huge.as_str()] {
        assert_eq!(source.resolve(name), Err(refused.clone()), "{name}");
    }
}

/// A loose object file the test kit writes by hand, in stored zlib blocks, reads back through gix
/// as git would have written it: one block or several, its Adler-32 and its id checked.
#[test]
fn a_loose_object_written_by_the_test_kit_reads_back() {
    let repository = git::empty_repository();
    let source = GixSource::open(&repository.dir).unwrap_or_else(|error| panic!("open: {error}"));
    for len in [5, 65_535, 65_536, 200_000] {
        let content: Vec<u8> = (0..len).map(|i| u8::try_from(i % 251).unwrap()).collect();
        let id = swarmops_git::git_hash(RawKind::Blob, &content);
        let mut file = format!("blob {len}\0").into_bytes();
        file.extend_from_slice(&content);
        git::write_loose_object(&repository.dir, &id.to_string(), &file);
        let size = u64::try_from(len).unwrap();
        assert_eq!(source.header(id), Ok((RawKind::Blob, size)), "{len}");
        assert_eq!(source.read(id), Ok((RawKind::Blob, content)), "{len}");
    }
}

#[test]
fn gix_source_refuses_what_it_does_not_read() {
    // (the repository's `config`, what opening it gives)
    let partial = || {
        Err(SourceError::Unsupported(Unsupported::PartialClone(
            "origin".to_owned(),
        )))
    };
    let configs: [(&str, Result<(), SourceError>); 6] = [
        (
            "[core]\n\trepositoryformatversion = 1\n\tbare = true\n\
             [extensions]\n\tobjectFormat = sha256\n",
            Err(SourceError::Unsupported(Unsupported::ObjectFormat(
                "sha256".to_owned(),
            ))),
        ),
        (
            "[core]\n\trepositoryformatversion = 1\n\tbare = true\n\
             [extensions]\n\trefStorage = reftable\n",
            Err(SourceError::Unsupported(Unsupported::RefStorage(
                "reftable".to_owned(),
            ))),
        ),
        // The files of `refs/` and `packed-refs` are what gix reads; naming them changes nothing,
        // nor do the extensions that change no read, nor a remote that is no promisor.
        (
            "[core]\n\trepositoryformatversion = 1\n\tbare = true\n\
             [extensions]\n\trefStorage = files\n\tobjectFormat = sha1\n\
             \tworktreeConfig = true\n\tpreciousObjects = true\n\tnoop = true\n\
             [remote \"origin\"]\n\tpromisor = false\n",
            Ok(()),
        ),
        // A partial clone, by its extension or by its promisor remote, as git writes it.
        (
            "[core]\n\trepositoryformatversion = 1\n\tbare = true\n\
             [extensions]\n\tpartialclone = origin\n",
            partial(),
        ),
        (
            "[core]\n\trepositoryformatversion = 1\n\tbare = true\n\
             [remote \"origin\"]\n\tpromisor = true\n\tpartialclonefilter = blob:none\n",
            partial(),
        ),
        // An extension git knows of no reader for, as git itself refuses one.
        (
            "[core]\n\trepositoryformatversion = 1\n\tbare = true\n\
             [extensions]\n\tfrobnicate = true\n",
            Err(SourceError::Unsupported(Unsupported::Extension(
                "frobnicate".to_owned(),
            ))),
        ),
    ];
    for (config, expected) in configs {
        let fixture = git::fixture("linear");
        fs::write(repo(&fixture).join("config"), config).unwrap();
        assert_eq!(
            GixSource::open(repo(&fixture)).map(|_| ()),
            expected,
            "{config}"
        );
    }

    // A promisor remote in `config.worktree`, which gix reads past the open (with
    // `extensions.worktreeConfig`) and the check before it does not: refused after the open.
    let fixture = git::fixture("linear");
    fs::write(
        repo(&fixture).join("config"),
        "[core]\n\trepositoryformatversion = 1\n\tbare = true\n\
         [extensions]\n\tworktreeConfig = true\n",
    )
    .unwrap();
    let worktree = "[remote \"origin\"]\n\tpromisor = true\n";
    fs::write(repo(&fixture).join("config.worktree"), worktree).unwrap();
    assert_eq!(GixSource::open(repo(&fixture)).map(|_| ()), partial());

    // A shallow clone: a `shallow` file naming the commits whose parents it lacks.
    let fixture = git::fixture("linear");
    let root = &manifest(&fixture).commits[0].oid;
    fs::write(repo(&fixture).join("shallow"), format!("{root}\n")).unwrap();
    assert_eq!(
        GixSource::open(repo(&fixture)).map(|_| ()),
        Err(SourceError::Unsupported(Unsupported::Shallow))
    );

    // No repository: the fixture's directory, which holds one, and an empty directory.
    let empty = tempfile::tempdir().unwrap();
    for path in [fixture.dir.as_path(), empty.path()] {
        assert!(
            matches!(GixSource::open(path), Err(SourceError::NotARepo { .. })),
            "{}",
            path.display()
        );
    }
}

#[test]
fn a_reftable_repository_is_unsupported() {
    // Only git 2.45 and newer keep refs in a reftable (`reftable.sh` needs it): the version
    // matrix's newest git builds the fixture and hands it over in `SWARMOPS_GIT_FIXTURES_DIR`.
    // The archives, which the declared git 2.43.0 made, lack it.
    if let Some(why) = git::skipped("reftable") {
        eprintln!("a_reftable_repository_is_unsupported: no reftable fixture at hand: {why}");
        return;
    }
    let fixture = git::fixture("reftable");
    // git kept the refs in a reftable, not as files with a configuration that says otherwise.
    assert!(repo(&fixture).join("reftable/tables.list").is_file());
    assert_eq!(
        GixSource::open(repo(&fixture)).map(|_| ()),
        Err(SourceError::Unsupported(Unsupported::RefStorage(
            "reftable".to_owned()
        )))
    );
}

#[test]
fn replacement_refs_are_not_followed() {
    let fixture = git::fixture("linear");
    let commits = &manifest(&fixture).commits;
    let (c2, c3) = (&commits[1], &commits[2]);
    let replace = repo(&fixture).join("refs/replace");
    fs::create_dir_all(&replace).unwrap();
    fs::write(replace.join(&c2.oid), format!("{}\n", c3.oid)).unwrap();
    let config = repo(&fixture).join("config");
    let mut text = fs::read_to_string(&config).unwrap();
    // git reads `refs/replace/<c2>` in place of `c2` unless told otherwise; and gix 0.89 loaded
    // the replacements when the repository's configuration said `core.useReplaceRefs = false`.
    for told in ["", "[core]\n\tuseReplaceRefs = false\n"] {
        text.push_str(told);
        fs::write(&config, &text).unwrap();
        let source = open(&fixture);
        assert!(
            source.read(oid(&c2.oid)) == Ok((RawKind::Commit, c2.raw.clone())),
            "{told:?}: c2 reads as another commit"
        );
        let size = u64::try_from(c2.raw.len()).unwrap();
        assert_eq!(
            source.header(oid(&c2.oid)),
            Ok((RawKind::Commit, size)),
            "{told:?}"
        );
    }
}

#[test]
fn an_object_the_repository_lacks_is_missing() {
    let fixture = git::fixture("linear");
    let source = open(&fixture);
    let absent = oid("0123456789abcdef0123456789abcdef01234567");
    assert_eq!(
        source.read(absent),
        Err(SourceError::Missing { oid: absent })
    );
    assert_eq!(
        source.header(absent),
        Err(SourceError::Missing { oid: absent })
    );
}

#[test]
fn the_real_unsupported_fixtures_are_refused() {
    use swarmops_git::RawTree;

    // Refused as they open: a SHA-256 repository, and a shallow and a partial clone git made.
    for (name, refused) in [
        ("sha256", Unsupported::ObjectFormat("sha256".to_owned())),
        ("shallow", Unsupported::Shallow),
        (
            "partial_clone",
            Unsupported::PartialClone("origin".to_owned()),
        ),
    ] {
        let fixture = git::fixture(name);
        assert_eq!(
            GixSource::open(repo(&fixture)).map(|_| ()),
            Err(SourceError::Unsupported(refused)),
            "{name}"
        );
    }

    // `git cat-file`'s source refuses the partial clone as gix's does.
    let partial = git::fixture("partial_clone");
    let refused = Unsupported::PartialClone("origin".to_owned());
    assert_eq!(
        CatFileSource::open(repo(&partial)).map(|_| ()),
        Err(SourceError::Unsupported(refused))
    );

    // A loose blob whose file holds another blob is damaged, once read.
    let corrupt = git::fixture("corrupt_object");
    let (source, held) = (open(&corrupt), manifest(&corrupt));
    let tree = &held.trees[&held.commits[1].tree];
    let damaged = (RawTree::parse(tree).unwrap().entries.into_iter())
        .find(|entry| entry.name == b"damaged.txt")
        .expect("the second commit adds damaged.txt")
        .oid;
    assert!(
        matches!(source.read(damaged), Err(SourceError::Corrupt { oid, .. }) if oid == damaged),
        "{damaged}"
    );
}

/// The fixtures both sources read: SHA-1, not shallow, no partial clone, refs kept as files.
fn readable(name: &str, manifest: &Manifest, repo: &Path) -> bool {
    manifest.object_format == "sha1"
        && !manifest.shallow
        && name != PARTIAL_CLONE
        && !repo.join("reftable").is_dir()
}

/// The fixture of a partial clone, which gix refuses as it opens.
const PARTIAL_CLONE: &str = "partial_clone";

/// Every object of every fixture both sources read reads the same through `git cat-file` as
/// through gix: its header and its bytes, or the same refusal (`corrupt_object`'s); `HEAD`
/// resolves alike; an absent id is missing from both.
#[test]
fn cat_file_source_reads_what_gix_reads() {
    let mut read = 0;
    for name in git::names() {
        let fixture = git::fixture(&name);
        let (Some(repo), Some(manifest)) = (&fixture.repo, &fixture.manifest) else {
            continue;
        };
        if !readable(&name, manifest, repo) {
            continue;
        }
        let gix = GixSource::open(repo).unwrap_or_else(|error| panic!("{name}: {error}"));
        let cat = CatFileSource::open(repo).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(cat.object_format(), ObjectFormat::Sha1, "{name}");
        assert_eq!(cat.resolve("HEAD"), gix.resolve("HEAD"), "{name}");

        // 1. Every object the manifest names, header and bytes.
        let commits = manifest.commits.iter().map(|commit| oid(&commit.oid));
        let trees = manifest.trees.keys().map(|hex| oid(hex));
        let blobs = manifest.blobs.keys().map(|hex| oid(hex));
        for id in commits.chain(trees).chain(blobs) {
            assert_eq!(cat.header(id), gix.header(id), "{name}: {id}");
            let got = cat.read(id);
            assert!(got == gix.read(id), "{name}: {id}");
            read += usize::from(got.is_ok());
        }

        // 2. An id no object has.
        let absent = oid("0123456789abcdef0123456789abcdef01234567");
        let missing = SourceError::Missing { oid: absent };
        assert_eq!(cat.header(absent), Err(missing.clone()), "{name}");
        assert_eq!(cat.read(absent), Err(missing), "{name}");
    }
    assert!(read > 0, "no fixture object was read");
}

/// The objects of a quarantine are found (by git) and read (loose, by gix) before the
/// repository's, and the repository's still through it; the repository alone lacks the
/// quarantine's.
#[test]
fn cat_file_source_reads_a_quarantine() {
    let repository = git::empty_repository();
    let held = git::empty_repository();
    let quarantine = held.dir.join("objects");

    // 1. One blob in each.
    let mut ids = Vec::new();
    for (dir, content) in [
        (&repository.dir, &b"served\n"[..]),
        (&held.dir, b"pushed\n"),
    ] {
        let id = swarmops_git::git_hash(RawKind::Blob, content);
        let mut file = format!("blob {}\0", content.len()).into_bytes();
        file.extend_from_slice(content);
        git::write_loose_object(dir, &id.to_string(), &file);
        ids.push((id, content.to_vec()));
    }
    let [(served, served_bytes), (pushed, pushed_bytes)] = <[_; 2]>::try_from(ids).unwrap();

    // 2. Both, through the quarantine; the pushed one missing without it.
    let both = CatFileSource::open_quarantine(&repository.dir, &quarantine)
        .unwrap_or_else(|error| panic!("open: {error}"));
    let size = u64::try_from(pushed_bytes.len()).unwrap();
    assert_eq!(both.header(pushed), Ok((RawKind::Blob, size)));
    assert_eq!(both.read(pushed), Ok((RawKind::Blob, pushed_bytes)));
    assert_eq!(both.read(served), Ok((RawKind::Blob, served_bytes)));
    let alone = CatFileSource::open(&repository.dir).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        alone.header(pushed),
        Err(SourceError::Missing { oid: pushed })
    );
}

/// A loose blob whose file holds another blob is `Corrupt` through `git cat-file`, as through
/// gix, and the source reads on after it; so is one whose header claims more than it holds.
#[test]
fn a_damaged_object_is_corrupt() {
    use swarmops_git::RawTree;

    // 1. The fixture's blob that holds another's bytes.
    let corrupt = git::fixture("corrupt_object");
    let (held, repo) = (manifest(&corrupt), repo(&corrupt));
    let source = CatFileSource::open(repo).unwrap_or_else(|error| panic!("open: {error}"));
    let tree = &held.trees[&held.commits[1].tree];
    let damaged = (RawTree::parse(tree).unwrap().entries.into_iter())
        .find(|entry| entry.name == b"damaged.txt")
        .expect("the second commit adds damaged.txt")
        .oid;
    let got = source.read(damaged);
    assert!(
        matches!(got, Err(SourceError::Corrupt { oid, .. }) if oid == damaged),
        "{damaged}: {got:?}"
    );
    let commit = oid(&held.commits[1].oid);
    let kind = source.read(commit).map(|(kind, _)| kind);
    assert_eq!(kind, Ok(RawKind::Commit));

    // 2. A loose blob whose header says it is longer than what follows: git's `cat-file` streams
    //    what follows and waits for its next request, so a reader of the rest would wait too.
    let repository = git::empty_repository();
    let content = b"short";
    let id = swarmops_git::git_hash(RawKind::Blob, content);
    let mut file = format!("blob {}\0", content.len() + 5).into_bytes();
    file.extend_from_slice(content);
    git::write_loose_object(&repository.dir, &id.to_string(), &file);
    let source = CatFileSource::open(&repository.dir).unwrap_or_else(|error| panic!("{error}"));
    let got = source.read(id);
    assert!(
        matches!(got, Err(SourceError::Corrupt { oid, .. }) if oid == id),
        "{id}: {got:?}"
    );
}

/// A ref whose loose file names an object stored as another kind is `Corrupt`, not that kind:
/// the tip is read and hashed before its kind is trusted.
#[test]
fn resolve_hashes_the_tip_before_it_trusts_its_kind() {
    let repository = git::empty_repository();
    let tip = "ab".repeat(20);
    git::write_loose_object(&repository.dir, &tip, b"blob 3\0abc");
    fs::write(repository.dir.join("refs/heads/main"), format!("{tip}\n")).unwrap();
    let source = GixSource::open(&repository.dir).unwrap_or_else(|error| panic!("open: {error}"));
    for name in ["main", tip.as_str()] {
        let got = source.resolve(name);
        assert!(
            matches!(&got, Err(SourceError::Corrupt { oid: named, .. }) if *named == oid(&tip)),
            "{name}: {got:?}"
        );
    }
}

/// A ref gix cannot decode is refused without its content quoted whole: the reason is bounded.
#[test]
fn a_ref_gix_cannot_decode_is_refused_with_a_bounded_reason() {
    const CONTENT: usize = 1 << 20;
    let repository = git::empty_repository();
    let junk = vec![b'z'; CONTENT];
    fs::write(repository.dir.join("refs/heads/junk"), junk).unwrap();
    let source = GixSource::open(&repository.dir).unwrap_or_else(|error| panic!("open: {error}"));
    let got = source.resolve("junk");
    let Err(SourceError::Io { reason }) = &got else {
        panic!("{:.300}", format!("{got:?}"));
    };
    assert!(reason.len() < CONTENT / 64, "{} bytes", reason.len());
}

/// `fixture`'s repository moved into a work tree, as `<work>/.git`, its configuration made a
/// work tree's; the work tree's path.
fn work_tree(fixture: &GitFixture) -> std::path::PathBuf {
    let work = fixture.dir.join("work");
    fs::create_dir(&work).unwrap();
    let git_dir = work.join(".git");
    fs::rename(repo(fixture), &git_dir).unwrap();
    let config = fs::read_to_string(git_dir.join("config")).unwrap();
    fs::write(
        git_dir.join("config"),
        config.replace("bare = true", "bare = false"),
    )
    .unwrap();
    work
}

/// A work tree opens at its `.git`, and a linked work tree through its `.git` file and the
/// `commondir` of the git directory it names: its own `HEAD`, the common refs.
#[test]
fn gix_source_opens_a_work_tree_and_a_linked_one() {
    let fixture = git::fixture("linear");
    let commits = &manifest(&fixture).commits;
    let (first, main) = (
        oid(&commits[0].oid),
        oid(&manifest(&fixture).refs["refs/heads/main"]),
    );
    let work = work_tree(&fixture);
    let opened = GixSource::open(&work).unwrap_or_else(|error| panic!("open: {error}"));
    assert_eq!(opened.resolve("HEAD"), Ok(main));

    // 1. The linked work tree's git directory, under the common one's `worktrees/`.
    let linked = fixture.dir.join("linked");
    let admin = work.join(".git/worktrees/linked");
    fs::create_dir_all(&admin).unwrap();
    fs::write(admin.join("HEAD"), format!("{first}\n")).unwrap();
    fs::write(admin.join("commondir"), "../..\n").unwrap();
    fs::write(
        admin.join("gitdir"),
        format!("{}\n", linked.join(".git").display()),
    )
    .unwrap();
    fs::create_dir(&linked).unwrap();
    fs::write(
        linked.join(".git"),
        format!("gitdir: {}\n", admin.display()),
    )
    .unwrap();

    // 2. Its `HEAD` is its own; `main` the common directory's.
    let opened = GixSource::open(&linked).unwrap_or_else(|error| panic!("open: {error}"));
    assert_eq!(opened.resolve("HEAD"), Ok(first));
    assert_eq!(opened.resolve("main"), Ok(main));
}

/// A repository named by a path relative to the working directory opens as by its absolute one.
#[test]
fn gix_source_opens_a_relative_path() {
    let fixture = git::fixture("linear");
    let absolute = fs::canonicalize(repo(&fixture)).unwrap();
    let cwd = fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
    // `../` up to the root, then the absolute path without its `/`.
    let mut relative = std::path::PathBuf::new();
    for _ in cwd.components().skip(1) {
        relative.push("..");
    }
    relative.push(absolute.strip_prefix("/").unwrap());
    assert!(relative.is_relative());
    let main = oid(&manifest(&fixture).refs["refs/heads/main"]);
    let opened = GixSource::open(&relative).unwrap_or_else(|error| panic!("open: {error}"));
    assert_eq!(opened.resolve("HEAD"), Ok(main));
}

/// [`MemSource`], the tests' source in memory, reads what gix reads: every object of a fixture
/// put into it reads back with the header and the bytes gix gives, and a commit resolves by its
/// id as through gix.
#[test]
fn mem_source_reads_what_gix_reads() {
    let fixture = git::fixture("merges");
    let (gix, held) = (open(&fixture), manifest(&fixture));
    let mut mem = MemSource::default();
    let commits = held.commits.iter().map(|commit| oid(&commit.oid));
    let trees = held.trees.keys().map(|hex| oid(hex));
    let blobs = held.blobs.keys().map(|hex| oid(hex));
    let ids: Vec<GitOid> = commits.chain(trees).chain(blobs).collect();
    for &id in &ids {
        let (kind, bytes) = gix.read(id).unwrap_or_else(|error| panic!("{id}: {error}"));
        assert_eq!(mem.put(kind, bytes), id);
    }
    for &id in &ids {
        assert_eq!(mem.header(id), gix.header(id), "{id}");
        assert!(mem.read(id) == gix.read(id), "{id}");
    }
    let first = &held.commits[0].oid;
    assert_eq!(mem.resolve(first), gix.resolve(first));
}
