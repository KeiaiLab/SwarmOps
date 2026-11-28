//! The signed changeset (M42): how the changeset a land makes carries a signature, so that it
//! exports to the commit that was signed, in every store and in every run.
//!
//! A signature is over a commit's bytes, and the commit a changeset exports to is a function of
//! the changeset ([`crate::export_oid`]). So no signature is ever added to a changeset: a signed
//! land writes one changeset that is made with its signature, as the import of the signed commit
//! would make it.
//!
//! ```text
//!   U   the changeset the land would write unsigned: its parents final, no extra header
//!   P   the commit of U's root, U's parents' exports, U's author and committer,
//!       no extra header, and sealed_message(U)                       ── Sealer::payload
//!   S   what the signer's tool prints for P
//!   C   P with the header `gpgsig S` after the committer; its id is the commit's
//!   L   U with P's message, the header gpgsig, and `git.oid` = C's id  ── Sealer::seal
//! ```
//!
//! `L` needs no rule of its own. It holds [`GIT_OID`] and no `git.lossy`, so version 1 of
//! `git_oid` renders it as it renders any imported changeset: its message and its extra headers
//! verbatim ([`render_commit`](crate::render_commit)). Those bytes are `C`, so its export oid is
//! its [`GIT_OID`], kept ([`ExportedOid::kept`](crate::ExportedOid::kept)): on a native parent by
//! rendering, on a signed land or another unchanged import by the shortcut that takes the
//! [`GIT_OID`]. A build that knows nothing of signing reads `L` as a commit made in git on the
//! store's export and imported back, and exports the same commit.
//!
//! What follows from it:
//!
//! - The signed commit carries the land's trailers and no `SwarmOps-Changeset` line
//!   ([`sealed_message`](crate::sealed_message)): `L`'s id hashes the signature, which covers the
//!   message. The commit is found by its id, through the external id index: the land records the
//!   entry, and the export records one that is missing ([step 8](mod@crate::export)).
//! - `L` keeps the land's metadata beside [`GIT_OID`] (`lander`, `agent`, `land_key`,
//!   `squashed_from`), which the land's replay and the readers of a land need. A store that takes
//!   the commit through git without ever holding `L` imports another changeset of it, with
//!   [`GIT_OID`] alone. Both export to the one commit, so the histories do not diverge.
//! - A changeset that exists is never given a signature: its id, and so its commit, would change.
//!
//! [`Sealer`] is the git side of a land's seal ([`swarmops_repo::land::SealFn`]): the operations
//! layer asks it for the payload, runs the signer's tool, and gives it the signature. It also
//! gives a stored changeset's signed bytes and signature back for a check
//! ([`Sealer::signed_commit`]), a signed land's and an imported commit's alike. It derives the
//! export oids it needs as the export does, from the store's objects, taking a memo only where a
//! [`MemoKey`] authenticates it, and keeps what it derived for its run: one `Sealer` serves one
//! request, every attempt of a land or every link of a chain.

use std::collections::HashMap;

use swarmops_core::{Changeset, ChangesetId, TreeId};
use swarmops_repo::land::Seal;
use swarmops_repo::{Derivation, LocalRepo, RepoError};

use crate::export_oid::{self, ExportOids, OnMemo};
use crate::{
    ExportOid, ExportReader, GIT_OID, GitOid, MemoKey, OidCache, RawKind, SignatureKind, native,
    raw, render,
};

/// The key of the header that holds a commit's signature, and what the key of every signature
/// header starts with (`gpgsig-sha256`).
const GPGSIG: &[u8] = b"gpgsig";

/// The git side of a land's seal, and the reader of a stored changeset's signature: see [the
/// module documentation](self).
pub struct Sealer<'r> {
    repo: &'r LocalRepo,
    /// The key the memos it trusts and writes are made under, if any.
    key: Option<MemoKey>,
    /// The export oids this run derived, and those of the changesets it sealed.
    exports: ExportOids,
    /// The git oids of the trees and blobs this run read.
    cache: OidCache,
    /// The changesets this run sealed, each with the commit that was signed.
    sealed: HashMap<ChangesetId, GitOid>,
}

/// A stored changeset's commit as a check of its signature needs it ([`Sealer::signed_commit`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SignedCommit {
    /// The bytes the changeset exports to, without git's `commit <len>\0` header and without any
    /// header whose key starts with `gpgsig`: what its `gpgsig` signature covers.
    pub payload: Vec<u8>,
    /// The `gpgsig` header's value with a line break after it, as the signer's tool printed it.
    pub signature: Vec<u8>,
    /// The signature's kind by its first line; `None` for one that starts with no marker git
    /// writes. Nothing here verifies it.
    pub kind: Option<SignatureKind>,
}

impl SignedCommit {
    /// A signed commit of these fields.
    pub fn new(payload: Vec<u8>, signature: Vec<u8>, kind: Option<SignatureKind>) -> SignedCommit {
        SignedCommit {
            payload,
            signature,
            kind,
        }
    }
}

/// The commit an unsigned changeset is signed as, without the fields it takes from the changeset
/// itself (its author and its committer).
struct Unsigned {
    /// The unsigned changeset's id: what an error about its objects names.
    about: ChangesetId,
    /// The git oid of its root.
    tree: GitOid,
    /// Its parents' export oids, in their order.
    parents: Vec<GitOid>,
    /// The message the commit is signed with ([`native::sealed_message`]).
    message: Vec<u8>,
}

impl Unsigned {
    /// The commit's bytes for changeset `cs`, with `extra_headers` after its committer.
    fn bytes(&self, cs: &Changeset, extra_headers: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
        render::commit_bytes(
            self.tree,
            &self.parents,
            &cs.author,
            &cs.committer,
            extra_headers,
            &self.message,
        )
    }
}

impl<'r> Sealer<'r> {
    /// A sealer on `repo` that has derived nothing yet. With `key`, the store's memo key, it
    /// takes the export oid memos the key authenticates and writes what it derives under it, so
    /// that it reads no history below a changeset whose memo checks; without one it derives a
    /// parent's export oid from the whole history below it, once.
    pub fn new(repo: &'r LocalRepo, key: Option<MemoKey>) -> Sealer<'r> {
        let on_memo = key.clone().map_or(OnMemo::Check, OnMemo::Trusted);
        Sealer {
            repo,
            key,
            exports: ExportOids::new(on_memo),
            cache: OidCache::new(),
            sealed: HashMap::new(),
        }
    }

    /// The bytes the signer's tool signs for `unsigned`, the changeset a land would write
    /// unsigned: the commit of its root's tree, its parents' exports, its author and its
    /// committer, with no extra header (the changeset's own, which a land's has none of, are
    /// not rendered) and its [`sealed_message`](crate::sealed_message). The trees and blobs
    /// below its root must be in the store; its own object need not be. A parent is a changeset
    /// the store holds, or one this run sealed. [`RepoError::NotFound`] for any other parent; a
    /// memo that disagrees with the store's objects is [`RepoError::DerivedCorrupt`].
    pub fn payload(&mut self, unsigned: &Changeset) -> Result<Vec<u8>, RepoError> {
        Ok(self.unsigned(unsigned)?.bytes(unsigned, &[]))
    }

    /// The seal of `unsigned` for `signature`, what the signer's tool printed for `payload`, and
    /// the id of the commit that was signed: the commit's message, the signature as the one
    /// extra header (`gpgsig`, without the signature's last line break) and the commit's id as
    /// [`GIT_OID`]. [`Seal::apply`] of it makes the changeset the land writes, which exports to
    /// that commit; the sealer keeps the changeset's id, so that a changeset sealed on it in
    /// this run finds its parent's commit before any store holds it.
    ///
    /// `payload` must be [`Sealer::payload`] of `unsigned`: another is [`RepoError::Internal`],
    /// as is a seal whose changeset would not render to the commit. The signature itself is the
    /// caller's to read: nothing here verifies it.
    pub fn seal(
        &mut self,
        unsigned: &Changeset,
        payload: &[u8],
        signature: &[u8],
    ) -> Result<(Seal, GitOid), RepoError> {
        // 1. The commit that was signed, derived again from what the run kept: the payload must
        // be this changeset's.
        let commit = self.unsigned(unsigned)?;
        if commit.bytes(unsigned, &[]) != payload {
            let what = format!(
                "the payload is not the commit of changeset {}",
                commit.about
            );
            return Err(RepoError::Internal(what));
        }

        // 2. The commit: the payload with the signature as a header after the committer.
        let value = signature.strip_suffix(b"\n").unwrap_or(signature);
        let headers = vec![(GPGSIG.to_vec(), value.to_vec())];
        let bytes = commit.bytes(unsigned, &headers);
        let oid = export_oid::hashed(RawKind::Commit, &bytes, ExportOid::NAME, commit.about.0)?;

        // 3. The seal, and the changeset it makes, which must render to that commit: then its
        // export oid is its `git.oid`, by rendering or by the shortcut.
        let metadata = vec![(GIT_OID.to_owned(), oid.to_string().into_bytes())];
        let seal = Seal::new(commit.message.clone(), headers, metadata);
        let sealed = seal.clone().apply(unsigned)?;
        let id = ChangesetId::new(sealed.encode_hashed()?.0.id());
        if native::render_commit(id, &sealed, &commit.parents, commit.tree) != bytes {
            let what = format!("sealed changeset {id} does not render to commit {oid}");
            return Err(RepoError::Internal(what));
        }
        self.sealed.insert(id, oid);
        self.exports.sealed(id, oid);
        Ok((seal, oid))
    }

    /// The commit changeset `id` exports to: one this run sealed from what it kept, any other
    /// derived from the store's objects as the export derives it, with the history below it
    /// down to a memo the key authenticates. [`RepoError::NotFound`] for a changeset the store
    /// does not hold and this run did not seal.
    pub fn export_oid(&mut self, id: ChangesetId) -> Result<GitOid, RepoError> {
        if let Some(&oid) = self.sealed.get(&id) {
            return Ok(oid);
        }
        let repo = self.repo;
        let read = repo.snapshot()?;
        let reader = ExportReader::new(&read, repo.derived_store());
        let exported = self.exports.of(&read, &reader, &self.cache, id)?;
        Ok(exported.git_oid())
    }

    /// What a check of changeset `id`'s signature reads: the bytes it exports to without their
    /// signature headers, and the `gpgsig` header's value with a line break after it; `None`
    /// for a changeset with no `gpgsig` header. One function for a signed land and for a commit
    /// imported from git, whose headers the store keeps verbatim.
    ///
    /// Every header whose key starts with `gpgsig` is left out of the payload: git verifies a
    /// commit's `gpgsig` signature over the commit without them. Those of the key `gpgsig`
    /// itself give the signature, their values joined, each with its line break. Any other is
    /// no part of either: `gpgsig-sha256`, the signature over the commit's SHA-256 form, which
    /// a commit may hold beside its `gpgsig`. Every other header stays in the payload.
    ///
    /// The bytes are the changeset's rendering, so the signature of an import that exports to a
    /// new commit (one imported with a loss, or above such a one) does not cover them.
    /// [`RepoError::NotFound`] for a changeset the store does not hold.
    pub fn signed_commit(&mut self, id: ChangesetId) -> Result<Option<SignedCommit>, RepoError> {
        let stored = self.repo.changeset(id)?;
        let cs = stored
            .ok_or_else(|| RepoError::NotFound(id.to_string()))?
            .changeset;
        let mut signatures = Vec::new();
        let mut rest = Vec::with_capacity(cs.extra_headers.len());
        for (key, value) in &cs.extra_headers {
            if key.as_slice() == GPGSIG {
                signatures.push(value);
            } else if !key.starts_with(GPGSIG) {
                rest.push((key.clone(), value.clone()));
            }
        }
        if signatures.is_empty() {
            return Ok(None);
        }
        let (tree, parents) = self.rendering(id, &cs.parents, cs.root)?;
        let message = native::export_message(id, &cs);
        let (author, committer) = (&cs.author, &cs.committer);
        let payload = render::commit_bytes(tree, &parents, author, committer, &rest, &message);
        let mut signature = Vec::new();
        for value in signatures {
            signature.extend_from_slice(value);
            signature.push(b'\n');
        }
        let kind = raw::marked(&signature);
        Ok(Some(SignedCommit::new(payload, signature, kind)))
    }

    /// Leaves the export oid memos of changeset `id`, which this run sealed as the commit `oid`,
    /// once the frame that records it is durable: its `git_oid` memo, and with a memo key its
    /// memo under the key, so that the next land on it reads no history below it. A changeset
    /// this run did not seal is derived instead ([`Sealer::export_oid`]), which leaves its memos
    /// as it goes. A put that fails is dropped: the value is derived again.
    ///
    /// [`RepoError::DerivedCorrupt`] for a `git_oid` memo there already that names another
    /// commit; [`RepoError::Internal`] when `oid` is not the commit the changeset was sealed as,
    /// or exports to.
    pub fn remember(&mut self, id: ChangesetId, oid: GitOid) -> Result<(), RepoError> {
        let known = match self.sealed.get(&id) {
            Some(&sealed) => sealed,
            None => self.export_oid(id)?,
        };
        if known != oid {
            let what = format!("changeset {id} exports to commit {known}, not {oid}");
            return Err(RepoError::Internal(what));
        }
        if self.sealed.contains_key(&id) {
            let memo = self.repo.derived_store();
            export_oid::remember_sealed(memo, self.key.as_ref(), id, oid)?;
        }
        Ok(())
    }

    /// The commit `unsigned` is signed as: its root's tree, its parents' exports and its sealed
    /// message.
    fn unsigned(&mut self, unsigned: &Changeset) -> Result<Unsigned, RepoError> {
        let about = ChangesetId::new(unsigned.encode_hashed()?.0.id());
        let (tree, parents) = self.rendering(about, &unsigned.parents, unsigned.root)?;
        Ok(Unsigned {
            about,
            tree,
            parents,
            message: native::sealed_message(unsigned, &[]),
        })
    }

    /// What a changeset of `parents` and `root` renders over: the git oid of its root's tree and
    /// its parents' export oids, in one snapshot, each from this run's values, from a memo the
    /// key authenticates or from the store's objects. `about` is the changeset, for the error a
    /// wrong memo raises.
    fn rendering(
        &mut self,
        about: ChangesetId,
        parents: &[ChangesetId],
        root: TreeId,
    ) -> Result<(GitOid, Vec<GitOid>), RepoError> {
        let repo = self.repo;
        let read = repo.snapshot()?;
        let reader = ExportReader::new(&read, repo.derived_store());
        let mut oids = Vec::with_capacity(parents.len());
        for &parent in parents {
            let exported = self.exports.of(&read, &reader, &self.cache, parent)?;
            oids.push(exported.git_oid());
        }
        let memos = (about, self.key.as_ref());
        let tree = export_oid::keyed_tree_oid(&reader, &self.cache, memos, root)?;
        Ok((tree, oids))
    }
}
