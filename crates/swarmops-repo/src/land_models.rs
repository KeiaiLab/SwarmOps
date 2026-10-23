//! Prototypes of the four landing models M4 measures before it builds one (umbrella decision 10;
//! the M4 plan's Task 1): how a publish handles a target that moved after the land read it.
//! Behind the `test-hooks` feature, for the measurement harness and its tests only: M4 Task 5
//! builds the model the measurement chooses into the land's publish and deletes this module.
//!
//! A land reads the target's head, `onto`, and [`Repo::prepare`]s its changeset `L` there: its
//! blobs, trees and changeset object written and flushed, outside the lock. Then
//! [`Repo::publish_with`] publishes it in one write transaction by one of the [`Model`]s, and
//! says how long it held the lock ([`Timed`]: from `MetaStore::write` returning to the end of
//! the write).
//!
//! - [`Model::CasLoop`], the yardstick (ops spec §3 step 7 on M1.6's `commit --advance` frame):
//!   inserts `L` and moves the target from `onto` to it by compare-and-swap. A target that moved
//!   drops the write ([`Published::Moved`]): the lander re-merges and tries again.
//! - [`Model::Pushrebase`]: reads the target under the lock (`MetaWrite::get_ref`); at `onto`,
//!   it publishes as the CAS loop does. Moved, it walks first parents from the actual head back
//!   to `onto` with `MetaWrite::changeset`, at most [`MAX_WALK`] steps (past them, or with
//!   `onto` off that line: [`Published::Moved`]), and gathers the paths the deltas of those
//!   changesets touch, reading their objects under the lock. When none of `L`'s paths overlaps
//!   one of them (two paths overlap when they are equal or one is a directory prefix of the
//!   other), it applies `L`'s delta to the actual head's tree, writes and flushes the new trees
//!   and `L'` under the lock (I1: before the frame that names them), and inserts `L'`, whose one
//!   parent is the actual head, with the target's move to it ([`Published::Landed`],
//!   `relocated`): `L'`'s delta is `L`'s, so its delta hash is too. An overlap goes back to the
//!   lander ([`Published::Overlap`]).
//! - [`Model::Metalog`]: Pushrebase, and on an overlap it merges each overlapping path with
//!   [`toy_merge`] when the path is a regular file on all three sides (`onto`, `L`, the actual
//!   head) and at most [`MAX_MERGE_BYTES`] on each, and relocates with the merged blobs;
//!   otherwise [`Published::Overlap`].
//! - [`Model::OpHeads`]: first settles every recorded land, oldest record first, onto `main` by
//!   Metalog's rule: a settled one moves `main` and loses its op-head ref; one that does not
//!   settle keeps its changeset under `release/ophc-<rest>` instead, a conflict left as data for
//!   its lander. Then it records its own: inserts `L` on `onto` and creates its op-head ref,
//!   `release/oph-<rest>`, the name its lander put in `L`'s metadata under [`OP_HEAD_KEY`]
//!   ([`Published::Pending`]), dropping that land's conflicted ref of an earlier record in the
//!   same frame. No compare-and-swap of the target: [`Repo::settle_op_heads`] settles without
//!   recording. Op heads land on `main` only, and their production form needs a record that
//!   FORMAT 1 does not have (the M4 plan's Decision 1).

mod model;
mod relocate;

pub use model::{
    CONFLICTED_PREFIX, MAX_MERGE_BYTES, MAX_WALK, Model, OP_HEAD_KEY, OP_HEAD_PREFIX, Publication,
    Published, Timed,
};
pub use relocate::toy_merge;

// The module documentation's links only.
#[cfg(doc)]
use crate::repo::Repo;
