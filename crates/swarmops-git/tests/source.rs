//! The repository the import reads, through gix: every object of the fixtures read by its id, refs
//! resolved to commits as git resolves them, replacement refs never followed, and the repositories
//! it does not read refused when it opens them.

use std::fs;
use std::path::Path;

use swarmops_git::{GitOid, GitSource, GixSource, ObjectFormat, RawKind, SourceError, Unsupported};
use swarmops_testkit::git::{self, GitFixture, Manifest};

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
        // `a_reftable_repository_is_unsupported`): another object format, a shallow clone, and
        // refs git keeps in a reftable (its `reftable/` directory), which only a git of 2.45 or
        // newer makes.
        if manifest.object_format != "sha1" || manifest.shallow || repo.join("reftable").is_dir() {
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
    let configs: [(&str, Result<(), SourceError>); 4] = [
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
        // The files of `refs/` and `packed-refs` are what gix reads; naming them changes nothing.
        (
            "[core]\n\trepositoryformatversion = 1\n\tbare = true\n\
             [extensions]\n\trefStorage = files\n\tobjectFormat = sha1\n",
            Ok(()),
        ),
        // A partial clone's repository opens; its missing objects are `Missing` when read.
        (
            "[core]\n\trepositoryformatversion = 1\n\tbare = true\n\
             [extensions]\n\tpartialclone = origin\n",
            Ok(()),
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

    // Refused as they open: a SHA-256 repository, and a shallow clone git made.
    for (name, refused) in [
        ("sha256", Unsupported::ObjectFormat("sha256".to_owned())),
        ("shallow", Unsupported::Shallow),
    ] {
        let fixture = git::fixture(name);
        assert_eq!(
            GixSource::open(repo(&fixture)).map(|_| ()),
            Err(SourceError::Unsupported(refused)),
            "{name}"
        );
    }

    // A partial clone opens, and lacks every blob its trees name.
    let partial = git::fixture("partial_clone");
    let (source, held) = (open(&partial), manifest(&partial));
    let mut lacked = std::collections::BTreeSet::new();
    for bytes in held.trees.values() {
        for entry in RawTree::parse(bytes).unwrap().entries {
            if entry.mode == b"100644" {
                assert!(!held.blobs.contains_key(&entry.oid.to_string()));
                let missing = SourceError::Missing { oid: entry.oid };
                assert_eq!(source.read(entry.oid), Err(missing.clone()));
                assert_eq!(source.header(entry.oid), Err(missing));
                lacked.insert(entry.oid);
            }
        }
    }
    assert_eq!(
        lacked.len(),
        3,
        "the blobs of dir/1.txt, dir/2.txt and dir/3.txt"
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
