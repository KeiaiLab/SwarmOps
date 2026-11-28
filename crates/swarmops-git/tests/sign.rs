//! The signed changeset (M42): a land's changeset sealed with a signature over the commit it
//! exports to. The sealed changeset carries the signature as its `gpgsig` header, the commit's
//! trailers in its message and the commit's id as `git.oid`, which version 1 of `git_oid` already
//! exports byte for byte: the tests read it back through `ExportOid` and `render_commit`, the two
//! functions a build without signing has. The oids they pin are git's (SHA-1 over git's object
//! bytes, worked out apart from SwarmOps). Every test owns fresh stores.

use std::collections::HashSet;
use std::path::Path;

use swarmops_core::{Changeset, ChangesetId, DeltaOp, Hashed, Signature};
use swarmops_git::sign::{Sealer, SignedCommit};
use swarmops_git::sync::{self, DropReason};
use swarmops_git::{
    BranchUpdate, ExportOid, ExportReader, ExportRequest, ExportedOid, GIT_OID, GitOid, GitSink,
    GitSource, GixSink, GixSource, ImportRequest, MemoKey, RawCommit, RawIdent, RawKind, RawTree,
    RawTreeEntry, SignatureKind, commit_bytes, export, git_hash, import, register, render_commit,
    tree_oid,
};
use swarmops_repo::{CasResult, Change, LocalRepo, RawChangeset, RepoError};
use swarmops_store::{DerivedSlot, ExternalEntry, SYSTEM_GIT};
use swarmops_testkit::git;
use swarmops_testkit::repo::{new_repo, put, request};
use tempfile::TempDir;

use crate::support::counted_store;

/// Who moves the stores' refs in these tests.
const ACTOR: &str = "test/signer";

/// The agent whose change lands: the sealed changeset's author, and its `agent`.
const AGENT: &str = "claude-code/worker-17";

/// Who lands: the sealed changeset's committer, and its `lander`.
const LANDER: &str = "swarmops/lander";

/// The change's head, as the land's `squashed_from` names it: no store holds it.
const HEAD: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

/// What the signer's tool printed: an armor of fixed bytes, which nothing here verifies.
const SIGNATURE: &[u8] =
    b"-----BEGIN SSH SIGNATURE-----\nU1NIU0lHAAAAAQ==\n-----END SSH SIGNATURE-----\n";

/// [`SIGNATURE`] as a commit's `gpgsig` header holds it: without its last line break.
const HEADER: &[u8] =
    b"-----BEGIN SSH SIGNATURE-----\nU1NIU0lHAAAAAQ==\n-----END SSH SIGNATURE-----";

/// [`SIGNATURE`] as a commit's bytes hold it: a header line and its continuation lines.
const HEADER_LINES: &str =
    "gpgsig -----BEGIN SSH SIGNATURE-----\n U1NIU0lHAAAAAQ==\n -----END SSH SIGNATURE-----\n";

/// The agent's signature, as SwarmOps' operations sign a commit.
fn agent() -> Signature {
    Signature::new(
        AGENT.as_bytes().to_vec(),
        b"worker-17@claude-code.swarmops.local".to_vec(),
        1_700_000_000,
        0,
        false,
    )
    .unwrap()
}

/// The lander's signature: fixed, so that the same land makes the same changeset.
fn lander() -> Signature {
    Signature::new(
        LANDER.as_bytes().to_vec(),
        b"lander@swarmops.swarmops.local".to_vec(),
        1_700_000_100,
        0,
        false,
    )
    .unwrap()
}

/// A fresh store with `ExportOid` registered.
fn store() -> (TempDir, LocalRepo) {
    let (dir, _, mut repo) = new_repo();
    register(&mut repo).unwrap();
    (dir, repo)
}

/// Commits `changes` on `parents` with `message` and `metadata` (its keys ascending), written
/// and recorded by the agent: a native changeset's id.
fn native(
    repo: &LocalRepo,
    parents: &[ChangesetId],
    changes: Vec<Change>,
    message: &str,
    metadata: &[(&str, &str)],
) -> ChangesetId {
    let mut req = request(parents, changes, message);
    req.author = agent();
    req.committer = agent();
    req.metadata = (metadata.iter())
        .map(|&(key, value)| (key.to_owned(), value.as_bytes().to_vec()))
        .collect();
    repo.commit(req).unwrap().id
}

/// Changeset `id`, which `repo` holds.
fn changeset(repo: &LocalRepo, id: ChangesetId) -> Changeset {
    repo.changeset(id).unwrap().unwrap().changeset
}

/// The value of `key` in `cs`'s metadata.
fn metadata<'c>(cs: &'c Changeset, key: &str) -> Option<&'c [u8]> {
    (cs.metadata.iter())
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_slice())
}

/// The changeset a land of `changes` onto `parent` makes before it is sealed: the agent its
/// author, the lander its committer, no extra header, and a land's metadata (`land_key` when the
/// land has `key`). Its blobs and trees are in `repo`, as a land writes them before it asks for
/// a seal.
fn unsigned(
    repo: &LocalRepo,
    parent: ChangesetId,
    changes: Vec<Change>,
    message: &str,
    key: Option<&str>,
) -> Changeset {
    let mut req = request(&[parent], changes, message);
    req.author = agent();
    req.committer = lander();
    let mut metadata = vec![("agent", AGENT)];
    metadata.extend(key.map(|key| ("land_key", key)));
    metadata.extend([("lander", LANDER), ("squashed_from", HEAD)]);
    req.metadata = (metadata.into_iter())
        .map(|(key, value)| (key.to_owned(), value.as_bytes().to_vec()))
        .collect();
    changeset(repo, repo.commit(req).unwrap().id)
}

/// Records `cs` in `repo` with the fields it has: its id, which is the one its bytes give.
fn record(repo: &LocalRepo, cs: &Changeset) -> ChangesetId {
    let raw = RawChangeset::new(
        cs.parents.clone(),
        cs.root,
        cs.author.clone(),
        cs.committer.clone(),
        cs.message.clone(),
        cs.extra_headers.clone(),
        cs.metadata.clone(),
    );
    let id = repo.commit_raw(raw).unwrap().outcomes[0].id;
    assert_eq!(id, cs.id().unwrap(), "the store made another changeset");
    id
}

/// Seals `unsigned` with [`SIGNATURE`]: the sealed changeset, not recorded, and its commit.
fn seal(sealer: &mut Sealer<'_>, unsigned: &Changeset) -> (Changeset, GitOid) {
    let payload = sealer.payload(unsigned).unwrap();
    let (seal, oid) = sealer.seal(unsigned, &payload, SIGNATURE).unwrap();
    (seal.apply(unsigned).unwrap(), oid)
}

/// Seals `unsigned` with [`SIGNATURE`] and records the sealed changeset in `repo`: its id and
/// its commit.
fn sealed(
    repo: &LocalRepo,
    sealer: &mut Sealer<'_>,
    unsigned: &Changeset,
) -> (ChangesetId, GitOid) {
    let (land, oid) = seal(sealer, unsigned);
    (record(repo, &land), oid)
}

/// Changeset `id`'s export oid, as `Repo::derived` gives it.
fn derived(repo: &LocalRepo, id: ChangesetId) -> ExportedOid {
    repo.derived::<ExportOid>(id, &()).unwrap()
}

/// The git oid of the tree `cs`'s root renders to.
fn tree_of(repo: &LocalRepo, cs: &Changeset) -> GitOid {
    let read = repo.snapshot().unwrap();
    tree_oid(&ExportReader::new(&read, repo.derived_store()), cs.root).unwrap()
}

/// `payload`, a commit's bytes, with [`HEADER_LINES`] after its last header.
fn with_header(payload: &[u8]) -> Vec<u8> {
    let blank = payload.windows(2).position(|pair| pair == b"\n\n");
    let at = blank.expect("a blank line ends the headers") + 1;
    [&payload[..at], HEADER_LINES.as_bytes(), &payload[at..]].concat()
}

/// Moves `main` of `repo` from `from` to `to`.
fn move_main(repo: &LocalRepo, from: Option<ChangesetId>, to: ChangesetId) {
    let moved = repo.cas_ref("main", from, Some(to), ACTOR).unwrap();
    assert!(matches!(moved.result, CasResult::Applied { .. }));
}

/// Exports `main` of `repo` into the branch `main` of the repository at `dir`: the commit the
/// branch names then.
fn export_main(repo: &LocalRepo, dir: &Path) -> GitOid {
    let mut sink = GixSink::open(dir).unwrap();
    let report = export(repo, &mut sink, &ExportRequest::new("main", "main"));
    report
        .unwrap_or_else(|error| panic!("export: {error}"))
        .head_oid
}

/// Imports what the branch `main` of the repository at `dir` reaches into `repo`, moving no ref,
/// as a fetch does: how many commits became changesets, and the tip's changeset.
fn fetch_main(repo: &LocalRepo, dir: &Path) -> (u64, ChangesetId) {
    let source = GixSource::open(dir).unwrap();
    let report = import(repo, &source, &ImportRequest::objects_only("main"));
    let report = report.unwrap_or_else(|error| panic!("import: {error}"));
    (report.imported, report.head)
}

/// The changeset objects of `ids` in `repo`, in their order, and the blobs their deltas put,
/// each once: what a native sync sends.
fn objects_of(repo: &LocalRepo, ids: &[ChangesetId]) -> (Vec<Hashed>, Vec<Hashed>) {
    let mut changesets = Vec::new();
    let mut blobs = Vec::new();
    let mut seen = HashSet::new();
    for &id in ids {
        let (kind, payload) = repo.object(id.0).unwrap().expect("the source holds it");
        let changeset = Changeset::decode(&payload).unwrap();
        changesets.push(Hashed::new(kind, payload));
        for entry in &changeset.delta {
            let DeltaOp::Put { id: blob, .. } = entry.op else {
                continue;
            };
            if seen.insert(blob) {
                let (kind, payload) = repo.object(blob.0).unwrap().expect("its blob");
                blobs.push(Hashed::new(kind, payload));
            }
        }
    }
    (changesets, blobs)
}

/// Takes changesets `ids` of `from` into `to` with their ids, as a native sync does: no external
/// id entry comes with them.
fn adopt(to: &LocalRepo, from: &LocalRepo, ids: &[ChangesetId]) {
    let (changesets, blobs) = objects_of(from, ids);
    to.put_blobs(blobs).unwrap();
    assert_eq!(to.adopt(changesets).unwrap().adopted, ids);
}

/// Writes into the repository at `dir` a commit made in git on `parent`, whose tree is
/// `parent`'s with one more file, `file` (holding its own name), and moves `main` to it.
fn commit_in_git(dir: &Path, parent: GitOid, file: &str) -> GitOid {
    let mut sink = GixSink::open(dir).unwrap();
    let (_, bytes) = sink.read(parent).unwrap();
    let (_, bytes) = sink.read(RawCommit::parse(&bytes).unwrap().tree).unwrap();
    let mut tree = RawTree::parse(&bytes).unwrap();
    let blob = sink.write_object(RawKind::Blob, file.as_bytes()).unwrap();
    let entry = RawTreeEntry::new(b"100644".to_vec(), file.as_bytes().to_vec(), blob);
    tree.entries.push(entry);
    tree.entries.sort_by(|a, b| a.name.cmp(&b.name));
    let tree = sink.write_object(RawKind::Tree, &tree.render()).unwrap();
    let person = RawIdent::new(
        b"A Git User".to_vec(),
        b"git@example.com".to_vec(),
        1_760_000_100,
        3_600,
        false,
    );
    let message = format!("Add {file}\n").into_bytes();
    let commit = RawCommit::new(
        tree,
        vec![parent],
        person.clone(),
        person,
        Vec::new(),
        message,
    );
    let commit = sink
        .write_object(RawKind::Commit, &commit.render())
        .unwrap();
    sink.sync(&[blob, tree, commit]).unwrap();
    let moved = sink.cas_branch("main", Some(parent), commit);
    assert_eq!(moved, Ok(BranchUpdate::Applied));
    commit
}

/// The message of the commits [`signed_in_git`] makes.
const IN_GIT: &[u8] = b"Signed in git\n";

/// Records in `repo` the changeset an import makes of a root commit signed in git, which holds
/// `base`'s tree: the lander its author and committer, `headers` its extra headers, [`IN_GIT`]
/// its message, and the commit's id its [`GIT_OID`]. The changeset's id.
fn signed_in_git(
    repo: &LocalRepo,
    base: ChangesetId,
    headers: &[(Vec<u8>, Vec<u8>)],
) -> ChangesetId {
    let base = changeset(repo, base);
    let (tree, person) = (tree_of(repo, &base), lander());
    let commit = commit_bytes(tree, &[], &person, &person, headers, IN_GIT);
    let made = git_hash(RawKind::Commit, &commit).to_string().into_bytes();
    let imported = Changeset::new(
        Vec::new(),
        base.root,
        base.delta,
        person.clone(),
        person,
        IN_GIT.to_vec(),
        headers.to_vec(),
        vec![(GIT_OID.to_owned(), made)],
    )
    .unwrap();
    record(repo, &imported)
}

#[test]
fn a_sealed_changeset_exports_to_the_commit_that_was_signed() {
    let (_dir, repo) = store();
    let base = native(&repo, &[], vec![put("a", b"a\n")], "Add a\n", &[]);
    let unsigned = unsigned(&repo, base, vec![put("b", b"b\n")], "Add b\n", Some("k-1"));
    let mut sealer = Sealer::new(&repo, None);
    let payload = sealer.payload(&unsigned).unwrap();
    let (seal, oid) = sealer.seal(&unsigned, &payload, SIGNATURE).unwrap();
    let land = seal.apply(&unsigned).unwrap();

    // 1. What was signed: the commit of the unsigned changeset's tree, parent and signatures,
    // with no extra header, and the land's trailers after its message, its own id not among
    // them.
    let (parent, tree) = (derived(&repo, base).git_oid(), tree_of(&repo, &unsigned));
    let message = format!("Add b\n\nSwarmOps-Land-Key: k-1\nSwarmOps-Squashed-From: {HEAD}\n");
    let (author, committer) = (&unsigned.author, &unsigned.committer);
    let signed = commit_bytes(tree, &[parent], author, committer, &[], message.as_bytes());
    assert_eq!(payload, signed);

    // 2. The sealed changeset: the unsigned one's parents, root, delta and signatures; that
    // message, the signature as its one header, and the commit's id beside the land's metadata.
    assert_eq!(
        (
            &land.parents,
            land.root,
            &land.delta,
            &land.author,
            &land.committer
        ),
        (
            &unsigned.parents,
            unsigned.root,
            &unsigned.delta,
            author,
            committer
        )
    );
    assert_eq!(land.message, message.as_bytes());
    assert_eq!(land.extra_headers, [(b"gpgsig".to_vec(), HEADER.to_vec())]);
    let keys: Vec<&str> = land.metadata.iter().map(|(key, _)| key.as_str()).collect();
    let expected = ["agent", "git.oid", "land_key", "lander", "squashed_from"];
    assert_eq!(keys, expected);
    assert_eq!(metadata(&land, GIT_OID), Some(oid.to_string().as_bytes()));
    assert_eq!(metadata(&land, "lander"), Some(LANDER.as_bytes()));

    // 3. Recorded, it exports to that commit, its own: the bytes that were signed with the
    // signature as a header after the committer.
    let id = record(&repo, &land);
    assert_eq!(derived(&repo, id), ExportedOid::new(oid.0, true));
    let rendered = render_commit(id, &land, &[parent], tree);
    assert_eq!(rendered, with_header(&payload));
    assert_eq!(git_hash(RawKind::Commit, &rendered), oid);
    assert_eq!(sealer.export_oid(id).unwrap(), oid);
}

#[test]
fn on_a_sealed_parent_it_exports_the_same() {
    let (_dir, repo) = store();
    let base = native(&repo, &[], vec![put("a", b"a\n")], "Add a\n", &[]);
    let mut sealer = Sealer::new(&repo, None);
    let first = unsigned(&repo, base, vec![put("b", b"b\n")], "Add b\n", Some("k-1"));
    let (first, first_oid) = sealed(&repo, &mut sealer, &first);

    // 1. A land on a signed land: its parent is an import that exports to its own commit, so
    // its export oid is its `git.oid`, taken without a tree read, and its rendering is that
    // commit all the same.
    let second = unsigned(&repo, first, vec![put("c", b"c\n")], "Add c\n", Some("k-2"));
    let (land, oid) = seal(&mut sealer, &second);
    let id = record(&repo, &land);
    assert_eq!(derived(&repo, id), ExportedOid::new(oid.0, true));
    let rendered = render_commit(id, &land, &[first_oid], tree_of(&repo, &land));
    assert_eq!(git_hash(RawKind::Commit, &rendered), oid);
    let parents = RawCommit::parse(&rendered).unwrap().parents;
    assert_eq!(parents, [first_oid]);

    // 2. A chain sealed in one run: a link's parent is the sealed link before it, which no
    // store holds yet, so its commit comes from what the run sealed. Recorded afterwards, both
    // export to the commits that were signed.
    let (_dir, repo) = store();
    let base = native(&repo, &[], vec![put("a", b"a\n")], "Add a\n", &[]);
    let mut sealer = Sealer::new(&repo, None);
    let first = unsigned(&repo, base, vec![put("b", b"b\n")], "Add b\n", None);
    let (first, first_oid) = seal(&mut sealer, &first);
    let mut second = first.clone();
    second.parents = vec![first.id().unwrap()];
    second.delta = Vec::new();
    second.message = b"Change nothing\n".to_vec();
    second.extra_headers = Vec::new();
    second.metadata.retain(|(key, _)| key != GIT_OID);
    let payload = sealer.payload(&second).unwrap();
    let head = format!("tree {}\nparent {first_oid}\n", tree_of(&repo, &first));
    assert!(payload.starts_with(head.as_bytes()));
    let (second, second_oid) = seal(&mut sealer, &second);
    let (first, second) = (record(&repo, &first), record(&repo, &second));
    assert_eq!(derived(&repo, first), ExportedOid::new(first_oid.0, true));
    assert_eq!(derived(&repo, second), ExportedOid::new(second_oid.0, true));
}

#[test]
fn a_sealed_commit_parses_back_to_its_fields() {
    let (_dir, repo) = store();
    let base = native(&repo, &[], vec![put("a", b"a\n")], "Add a\n", &[]);
    let unsigned = unsigned(&repo, base, vec![put("b", b"b\n")], "Add b\n", Some("k-1"));
    let mut sealer = Sealer::new(&repo, None);
    let (land, oid) = seal(&mut sealer, &unsigned);
    let id = record(&repo, &land);
    let (parent, tree) = (derived(&repo, base).git_oid(), tree_of(&repo, &land));
    let rendered = render_commit(id, &land, &[parent], tree);

    // 1. git's fields of the commit: the land's, and the signature as the one extra header.
    let commit = RawCommit::parse(&rendered).unwrap();
    assert_eq!((commit.tree, &commit.parents[..]), (tree, &[parent][..]));
    let author = b"claude-code/worker-17 <worker-17@claude-code.swarmops.local> 1700000000 +0000";
    let committer = b"swarmops/lander <lander@swarmops.swarmops.local> 1700000100 +0000";
    assert_eq!(commit.author.render(), author);
    assert_eq!(commit.committer.render(), committer);
    assert_eq!(
        commit.extra_headers,
        [(b"gpgsig".to_vec(), HEADER.to_vec())]
    );
    assert_eq!(commit.message, land.message);

    // 2. It renders back to its own bytes, so an import of it loses nothing.
    assert_eq!(commit.render(), rendered);
    assert_eq!(git_hash(RawKind::Commit, &commit.render()), oid);
}

#[test]
fn the_sealed_bytes_are_pinned() {
    let (_dir, repo) = store();
    // The native changeset the export oid's tests pin, and its commit.
    let squashed = "c".repeat(64);
    let metadata = [
        ("agent", AGENT),
        ("land_key", "k-1"),
        ("run_id", "r1"),
        ("squashed_from", squashed.as_str()),
    ];
    let base = native(&repo, &[], vec![put("a", b"a\n")], "Add a\n", &metadata);
    let pinned = "038263fb99e3991842afb4918018810ed6345d9306c5d22ac82ea52b285e1290";
    assert_eq!(base.0.to_hex(), pinned);

    // A land of the file `b` onto it, sealed.
    let unsigned = unsigned(&repo, base, vec![put("b", b"b\n")], "Add b\n", Some("k-2"));
    let mut sealer = Sealer::new(&repo, None);
    let (land, oid) = seal(&mut sealer, &unsigned);
    let id = record(&repo, &land);
    let parent: GitOid = "289054922b69c2600fa2e17c8990bfaaa2654b1b".parse().unwrap();
    let tree: GitOid = "3683f870be446c7cc05ffaef9fa06415276e1828".parse().unwrap();
    let expected = format!(
        "tree 3683f870be446c7cc05ffaef9fa06415276e1828\n\
         parent 289054922b69c2600fa2e17c8990bfaaa2654b1b\n\
         author claude-code/worker-17 <worker-17@claude-code.swarmops.local> 1700000000 +0000\n\
         committer swarmops/lander <lander@swarmops.swarmops.local> 1700000100 +0000\n\
         gpgsig -----BEGIN SSH SIGNATURE-----\n U1NIU0lHAAAAAQ==\n -----END SSH SIGNATURE-----\n\
         \n\
         Add b\n\
         \n\
         SwarmOps-Land-Key: k-2\n\
         SwarmOps-Squashed-From: {HEAD}\n"
    );
    let rendered = render_commit(id, &land, &[parent], tree);
    assert_eq!(String::from_utf8(rendered).unwrap(), expected);
    // Pinned: the oid git gives these bytes (SHA-1 over `commit 459\0` and the bytes above,
    // worked out apart from SwarmOps), and the sealed changeset's id, which the hash contract
    // fixes.
    assert_eq!(expected.len(), 459);
    assert_eq!(oid.to_string(), "8495b47b7664c2712c6d7731e3b6cd1e0805a36c");
    assert_eq!(derived(&repo, id), ExportedOid::new(oid.0, true));
    let pinned = "f2c07a8d1a1e30cdb9f81a8add2b5beecc7d28af3752afd50c8b9254b21b3cc7";
    assert_eq!(id.0.to_hex(), pinned);
}

#[test]
fn a_signed_commit_is_its_payload_and_its_signature() {
    let (_dir, repo) = store();
    let base = native(&repo, &[], vec![put("a", b"a\n")], "Add a\n", &[]);
    let unsigned = unsigned(&repo, base, vec![put("b", b"b\n")], "Add b\n", Some("k-1"));
    let mut sealer = Sealer::new(&repo, None);
    let payload = sealer.payload(&unsigned).unwrap();
    let (land, _) = sealed(&repo, &mut sealer, &unsigned);

    // 1. A signed land: what was signed, and the armor as the tool printed it.
    let mut reader = Sealer::new(&repo, None);
    let expected = SignedCommit::new(payload, SIGNATURE.to_vec(), Some(SignatureKind::Ssh));
    assert_eq!(reader.signed_commit(land).unwrap(), Some(expected));

    // 2. A changeset with no `gpgsig` header has no signature.
    assert_eq!(reader.signed_commit(base).unwrap(), None);

    // 3. A signed commit imported from git reads the same way: its bytes without the header,
    // every other header kept, and the header's value with its line break.
    let armor = b"-----BEGIN PGP SIGNATURE-----\n\nabc\n-----END PGP SIGNATURE-----";
    let encoding = (b"encoding".to_vec(), b"ISO-8859-1".to_vec());
    let signature = (b"gpgsig".to_vec(), armor.to_vec());
    let headers = [encoding.clone(), signature.clone()];
    let imported = signed_in_git(&repo, base, &headers);
    let signed = reader.signed_commit(imported).unwrap();
    let signed = signed.expect("a signature");
    let (tree, person) = (tree_of(&repo, &changeset(&repo, base)), lander());
    let unsigned = commit_bytes(tree, &[], &person, &person, &headers[..1], IN_GIT);
    assert_eq!(signed.payload, unsigned);
    assert_eq!(signed.signature, [&armor[..], b"\n"].concat());
    assert_eq!(signed.kind, Some(SignatureKind::Pgp));

    // 4. A commit that holds a second signature header beside it, `gpgsig-sha256` (the
    // signature over the commit's SHA-256 form): git verifies a `gpgsig` signature over the
    // commit without either header, so neither is in the payload, and the second one's value
    // is no part of the signature.
    let other = b"-----BEGIN SSH SIGNATURE-----\nxyz\n-----END SSH SIGNATURE-----";
    let second = (b"gpgsig-sha256".to_vec(), other.to_vec());
    let headers = [encoding.clone(), signature, second.clone()];
    let imported = signed_in_git(&repo, base, &headers);
    let signed = reader.signed_commit(imported).unwrap();
    let expected = SignedCommit::new(
        unsigned,
        [&armor[..], b"\n"].concat(),
        Some(SignatureKind::Pgp),
    );
    assert_eq!(signed, Some(expected));

    // 5. A commit with that second header alone has no signature: none over the bytes the
    // changeset exports to.
    let imported = signed_in_git(&repo, base, &[encoding, second]);
    assert_eq!(reader.signed_commit(imported).unwrap(), None);

    // 6. A changeset the store does not hold is not found.
    let missing = ChangesetId::new(swarmops_core::ObjectId::new([9; 32]));
    let error = reader.signed_commit(missing).unwrap_err();
    assert!(matches!(error, RepoError::NotFound(_)), "{error}");
}

#[test]
fn a_payload_of_another_changeset_is_not_sealed() {
    let (_dir, repo) = store();
    let base = native(&repo, &[], vec![put("a", b"a\n")], "Add a\n", &[]);
    let one = unsigned(&repo, base, vec![put("b", b"b\n")], "Add b\n", Some("k-1"));
    let two = unsigned(&repo, base, vec![put("c", b"c\n")], "Add c\n", Some("k-2"));
    let mut sealer = Sealer::new(&repo, None);
    let payload = sealer.payload(&one).unwrap();
    let error = sealer.seal(&two, &payload, SIGNATURE).unwrap_err();
    assert!(matches!(error, RepoError::Internal(_)), "{error}");
}

#[test]
fn a_remembered_seal_is_taken_from_its_memo() {
    // 1. Without a key, the sealed changeset's `git_oid` memo is left as a derivation leaves
    // it: the oid's 20 bytes, then `kept`.
    let (_dir, repo) = store();
    let base = native(&repo, &[], vec![put("a", b"a\n")], "Add a\n", &[]);
    let land = unsigned(&repo, base, vec![put("b", b"b\n")], "Add b\n", Some("k-1"));
    let mut sealer = Sealer::new(&repo, None);
    let (id, oid) = sealed(&repo, &mut sealer, &land);
    let slot = DerivedSlot::new("git_oid", 1, id.0, &[]);
    assert_eq!(repo.derived_store().get(&slot).unwrap(), None);
    sealer.remember(id, oid).unwrap();
    let stored = repo.derived_store().get(&slot).unwrap().unwrap();
    assert_eq!(stored, [&oid.0[..], &[1]].concat());

    // 2. A memo there already that names another commit is refused, as the export refuses it.
    let other = unsigned(&repo, id, vec![put("c", b"c\n")], "Add c\n", Some("k-2"));
    let (other, other_oid) = sealed(&repo, &mut sealer, &other);
    let slot = DerivedSlot::new("git_oid", 1, other.0, &[]);
    let wrong = [&oid.0[..], &[1]].concat();
    assert!(repo.derived_store().put(&slot, &wrong).unwrap());
    let error = sealer.remember(other, other_oid).unwrap_err();
    assert!(matches!(error, RepoError::DerivedCorrupt { .. }), "{error}");

    // 3. With a memo key, the memo under the key stops the walk: a later run reads no object to
    // know the sealed changeset's commit, however long the history below it is.
    let (_dir, _root, repo, reads) = counted_store();
    let key = MemoKey::from_bytes([7; 32]);
    let mut head = native(&repo, &[], vec![put("a", b"a\n")], "Add a\n", &[]);
    for n in 0..8_u8 {
        head = native(&repo, &[head], vec![put("a", &[n])], "Change a\n", &[]);
    }
    let land = unsigned(&repo, head, vec![put("b", b"b\n")], "Add b\n", Some("k-1"));
    let mut sealer = Sealer::new(&repo, Some(key.clone()));
    let (id, oid) = sealed(&repo, &mut sealer, &land);
    sealer.remember(id, oid).unwrap();
    reads.take();
    let mut later = Sealer::new(&repo, Some(key.clone()));
    assert_eq!(later.export_oid(id).unwrap(), oid);
    assert_eq!(
        reads.take(),
        0,
        "the memo under the key stands for the history"
    );

    // 4. So the next land on it reads its own new tree and blob, and no changeset.
    let next = unsigned(&repo, id, vec![put("c", b"c\n")], "Add c\n", Some("k-2"));
    reads.take();
    let mut later = Sealer::new(&repo, Some(key));
    let payload = later.payload(&next).unwrap();
    assert_eq!(reads.take(), 2, "the land's root and the blob it adds");
    let head = format!("tree {}\nparent {oid}\n", tree_of(&repo, &next));
    assert!(payload.starts_with(head.as_bytes()));
}

#[test]
fn two_changesets_of_one_signed_commit_do_not_diverge() {
    // With a land key the sync knows the land by its trailer; without one, as a merge that
    // changes nothing.
    for (key, reason) in [(Some("k-1"), DropReason::Trailer), (None, DropReason::NoOp)] {
        // 1. The store that lands: a native base and a signed land on it, both exported, so its
        // index names the land for the commit.
        let (_first_dir, first) = store();
        let remote = git::empty_repository();
        let base = native(&first, &[], vec![put("a", b"a\n")], "Add a\n", &[]);
        move_main(&first, None, base);
        let base_oid = export_main(&first, &remote.dir);
        let land = unsigned(&first, base, vec![put("b", b"b\n")], "Add b\n", key);
        let mut sealer = Sealer::new(&first, None);
        let (land, oid) = sealed(&first, &mut sealer, &land);
        move_main(&first, Some(base), land);
        assert_eq!(export_main(&first, &remote.dir), oid, "{key:?}");
        let entry = first.external_id(SYSTEM_GIT, &oid.0).unwrap();
        assert_eq!(entry, Some(ExternalEntry::new(land, true)), "{key:?}");

        // 2. A second store takes the base and exports it, then takes the land as a native sync
        // moves it: with its id, and with no entry for its commit.
        let (_second_dir, second) = store();
        let own = git::empty_repository();
        adopt(&second, &first, &[base]);
        move_main(&second, None, base);
        assert_eq!(export_main(&second, &own.dir), base_oid, "{key:?}");
        adopt(&second, &first, &[land]);
        move_main(&second, Some(base), land);
        assert_eq!(second.external_id(SYSTEM_GIT, &oid.0).unwrap(), None);

        // 3. A fetch of the commit there makes a second changeset of it, the commit's import:
        // its `git.oid` alone, on the same parent, with the same message and header.
        let (imported, twin) = fetch_main(&second, &remote.dir);
        assert_eq!(imported, 1, "{key:?}");
        assert_ne!(twin, land, "{key:?}");
        let (ours, theirs) = (changeset(&second, land), changeset(&second, twin));
        assert_eq!(
            theirs.metadata,
            [(GIT_OID.to_owned(), oid.to_string().into_bytes())]
        );
        assert_eq!(
            (
                &theirs.parents,
                theirs.root,
                &theirs.message,
                &theirs.extra_headers
            ),
            (&ours.parents, ours.root, &ours.message, &ours.extra_headers)
        );
        let entry = second.external_id(SYSTEM_GIT, &oid.0).unwrap();
        assert_eq!(entry, Some(ExternalEntry::new(twin, true)), "{key:?}");

        // 4. Both export to the one commit: the export of `main`, at the land, gives the same
        // commit id, the same bytes, and leaves the index's entry as it is.
        assert_eq!(derived(&second, land), derived(&second, twin), "{key:?}");
        assert_eq!(export_main(&second, &own.dir), oid, "{key:?}");
        let written = GixSource::open(&own.dir).unwrap().read(oid).unwrap();
        let fetched = GixSource::open(&remote.dir).unwrap().read(oid).unwrap();
        assert_eq!(written, fetched, "{key:?}");
        let entry = second.external_id(SYSTEM_GIT, &oid.0).unwrap();
        assert_eq!(entry, Some(ExternalEntry::new(twin, true)), "{key:?}");

        // 5. A commit made in git on it imports, on the changeset the index names.
        let made = commit_in_git(&remote.dir, oid, "c");
        let (imported, child) = fetch_main(&second, &remote.dir);
        assert_eq!(imported, 1, "{key:?}");
        assert_eq!(changeset(&second, child).parents, [twin], "{key:?}");

        // 6. A sync of `main`, at the land, with that remote head: the land is unpublished by
        // its id, the remote holds it all the same, and nothing is left to land: `main` goes
        // forward to the remote's head.
        let plan = sync::plan(&second, land, child).unwrap();
        assert_eq!(
            (plan.base, &plan.unpublished[..], plan.behind),
            (base, &[land][..], 2)
        );
        let recognised = sync::recognise(&second, &plan, child).unwrap();
        assert_eq!(recognised.dropped, [(land, reason)], "{key:?}");
        assert!(recognised.to_land.is_empty(), "{key:?}");
        let relocated = sync::relocate(&second, &recognised.to_land, child, &lander()).unwrap();
        assert_eq!(
            (relocated.head, relocated.landed.len()),
            (child, 0),
            "{key:?}"
        );

        // 7. In the store that made the land, the index names it, and no second changeset is
        // made: the commit made in git comes back on the land itself. So the import of that
        // commit is another changeset there than in the second store, as its parent is, and so
        // on up the history; each exports to the commit git made, as it is.
        let (imported, above) = fetch_main(&first, &remote.dir);
        assert_eq!(imported, 1, "{key:?}");
        assert_eq!(changeset(&first, above).parents, [land], "{key:?}");
        assert_ne!(above, child, "{key:?}");
        let kept = ExportedOid::new(made.0, true);
        assert_eq!(derived(&first, above), kept, "{key:?}");
        assert_eq!(derived(&second, child), kept, "{key:?}");
    }
}
