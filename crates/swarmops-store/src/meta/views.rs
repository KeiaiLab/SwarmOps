//! FORMAT 3's view work (the FORMAT 3 design §2.5 and its 2026-11-11 amendment): [`step`] builds
//! a run of the records the checkpoint passed past the runs, merges it at [`RUN_FANOUT`], and
//! publishes the new manifest, under `meta/views.lock` and never under `meta/lock`; of the
//! changeset records (domain 0), then of the fact rows (domain 1, §2.8).
//!
//! ```text
//! records the checkpoint passed:  [0 ............................................. records)
//! manifest before:                [run 0 ......... )[run 1 ...)        tail (> RUN_MIN)
//! the new run:                                                 [new ................)
//! merged while run 1 < 4 × new:                     [merged ............................)
//! ```
//!
//! Lock order everywhere: `maint.lock` → `views.lock` → `meta/lock` (rule 8). A step takes
//! `views.lock` without waiting and no other lock; a writer never takes `views.lock` while it
//! holds `meta/lock`. Who steps: a commit past [`super::runs::RUN_BACKLOG_MAX`] after it unlocked,
//! the caller of [`crate::MetaStore::step_views`] after it answered, and gc.

use std::fs::{self, File, TryLockError};
use std::io;
use std::ops::ControlFlow;

use swarmops_core::bytes;

use super::column::AppendOnlyColumn;
use super::facts;
use super::features;
use super::index::{DHS, IDS};
use super::journal::Journal;
use super::lock::{Create, open_flock_file};
use super::runs::{self, Domain, MAX_RUNS, Manifest, RUN_FANOUT, RUN_MIN, Run};
use crate::error::{FileOp, StoreError};
use crate::format::StoreFormat;
use crate::fs_ops::StoreFs;
use crate::layout::Layout;

/// What a view step did ([`crate::MetaStore::step_views`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Step {
    /// It held `meta/views.lock`: the runs cover what the checkpoint passed, but a tail of at
    /// most [`RUN_MIN`] records.
    Done,
    /// Another step held `meta/views.lock`, or the store is below FORMAT 3 and has no runs: it
    /// did nothing and did not wait. The next step builds.
    Skipped,
}

/// One step of `layout`'s view work, `journal` giving what the checkpoint passed: see the
/// module documentation. Below FORMAT 3, [`Step::Skipped`] and nothing touched.
///
/// 1. `meta/views.lock` by `try_lock`; held elsewhere, [`Step::Skipped`] at once. Then
///    `meta/features` read again: a feature this build does not support, listed since the store
///    was opened, is [`StoreError::TooNew`] before anything is written (G13).
/// 2. The runs outside the manifest, and what `tmp/` holds, removed: a step or a merge that
///    stopped part way left them (rule 7).
/// 3. For each domain, the changeset records and then the fact rows (§2.8), with more than
///    [`RUN_MIN`] of them past its runs: the run of them built in `tmp/`, synced and renamed
///    into place, merged with the run before it while that one is shorter than [`RUN_FANOUT`]
///    times it; the new manifest published by an atomic replace, and only then the runs it
///    replaced removed.
///
/// A manifest that covers more than the checkpoint passed is [`StoreError::IndexCorrupt`] (M2).
pub(crate) fn step(layout: &Layout, fs: &StoreFs, journal: &Journal) -> Result<Step, StoreError> {
    match layout.format() {
        StoreFormat::V1 | StoreFormat::V2 => return Ok(Step::Skipped),
        StoreFormat::V3 => {}
    }

    // 1. The lease, never waited for.
    let Some(_lease) = try_lease(layout, fs)? else {
        return Ok(Step::Skipped);
    };
    features::check(layout, fs)?;

    // 2. What the checkpoint passed, the manifest, and what earlier steps left.
    let ckpt = journal.ckpt3()?;
    let (_, mut manifest) = runs::read_manifest(layout, fs)?;
    sweep(layout, fs, &manifest)?;

    // 3. Each domain's tail past its runs, if long enough for a run of its own.
    for (domain, passed) in [
        (Domain::Changesets, ckpt.records),
        (Domain::Facts, ckpt.fact_rows),
    ] {
        manifest = step_domain(layout, fs, manifest, domain, passed)?;
    }
    Ok(Step::Done)
}

/// [`step`]'s work on `domain`, of which the checkpoint passed `passed` records: none with at
/// most [`RUN_MIN`] past its runs; else their run built, merged, the manifest after `manifest`
/// published and the runs it replaced removed. Returns the manifest now in place.
fn step_domain(
    layout: &Layout,
    fs: &StoreFs,
    manifest: Manifest,
    domain: Domain,
    passed: u64,
) -> Result<Manifest, StoreError> {
    // 1. The tail past the runs.
    let covered = manifest.covered(domain);
    let Some(tail) = passed.checked_sub(covered) else {
        return Err(StoreError::IndexCorrupt {
            what: "the manifest's runs cover records the checkpoint has not passed",
        });
    };
    if tail <= RUN_MIN {
        return Ok(manifest);
    }
    let (next, replaced) = merged(&manifest, domain, covered, passed);

    // 2. Built, published, and then the runs it replaced removed.
    let built = (next.runs.iter()).find(|run| run.domain == domain && run.to == passed);
    let built = built.expect("the new manifest ends at the checkpoint");
    build(layout, fs, built)?;
    let path = layout.views_dir().join(runs::MANIFEST);
    fs.atomic_replace(path.as_std_path(), &next.encode())?;
    for run in &replaced {
        remove(fs, &runs::run_path(layout, run))?;
    }
    if !replaced.is_empty() {
        fs.fsync_dir(layout.views_dir().as_std_path())?;
    }
    Ok(next)
}

/// The manifest after a run of `domain`'s records `[covered, records)` is added to `manifest`
/// and merged with the runs before it while each is shorter than [`RUN_FANOUT`] times it (rule
/// 5), or while the manifest would list more than [`MAX_RUNS`] (which rule 5 alone never
/// reaches: it keeps a domain within ⌈log₄(n / 1,024)⌉ + 1 runs); and the runs it replaced.
/// The runs stay in (domain, from) order.
fn merged(manifest: &Manifest, domain: Domain, covered: u64, records: u64) -> (Manifest, Vec<Run>) {
    let (mut kept, others): (Vec<Run>, Vec<Run>) =
        (manifest.runs.iter()).partition(|run| run.domain == domain);
    let mut new = Run {
        domain,
        from: covered,
        to: records,
    };
    let mut replaced = Vec::new();
    while let Some(&before) = kept.last()
        && (before.len() < RUN_FANOUT.saturating_mul(new.len())
            || kept.len() + others.len() >= MAX_RUNS)
    {
        new.from = before.from;
        replaced.push(before);
        kept.pop();
    }
    kept.push(new);
    kept.extend(others);
    kept.sort_unstable_by_key(|run| (run.domain, run.from));
    let next = Manifest {
        generation: manifest.generation + 1,
        runs: kept,
    };
    (next, replaced)
}

/// Builds `run`'s file from the index: of changeset records, the id and delta-hash columns of
/// its records read a window at a time; of fact rows, each row's subject and other, read from its
/// fact (all synced, below the checkpoint: no cut reaches them). Sorted, written to `tmp/` with
/// one append, synced, then renamed into place. The same records give the same bytes, so a file
/// a crash left under its name is replaced by its equal.
fn build(layout: &Layout, fs: &StoreFs, run: &Run) -> Result<(), StoreError> {
    // 1. The two sections.
    let [mut ids, mut dh] = match run.domain {
        Domain::Changesets => [
            sorted_column(layout, fs, IDS, run)?,
            sorted_column(layout, fs, DHS, run)?,
        ],
        Domain::Facts => facts::sections(layout, fs, run.from, run.to)?,
    };
    ids.sort_unstable();
    dh.sort_unstable();
    let twice = ids.windows(2).any(|pair| pair[0].0 == pair[1].0);
    if run.domain == Domain::Changesets && twice {
        return Err(StoreError::IndexCorrupt {
            what: "the index holds an id twice",
        });
    }

    // 2. Written in tmp/, synced, its entry there too (the rename syncs only sorted3/), renamed.
    let tmp_dir = layout.views_dir().join(runs::TMP);
    if !fs::exists(&tmp_dir).map_err(|e| fs.locate(FileOp::Stat, &tmp_dir, e))? {
        fs.create_dir_all(tmp_dir.as_std_path())?;
    }
    let tmp = runs::tmp_path(layout, &run.name());
    remove(fs, &tmp)?;
    fs.append(tmp.as_std_path(), &runs::encode_run(run, [&ids, &dh]))?;
    fs.sync_data(tmp.as_std_path())?;
    fs.fsync_dir(tmp_dir.as_std_path())?;
    fs.rename_durable(tmp.as_std_path(), runs::run_path(layout, run).as_std_path())?;
    Ok(())
}

/// The (key, rec) entries of `run`'s records in the index column `rel`, unsorted.
fn sorted_column(
    layout: &Layout,
    fs: &StoreFs,
    rel: &str,
    run: &Run,
) -> Result<Vec<([u8; 32], u64)>, StoreError> {
    let column =
        AppendOnlyColumn::<32>::open(fs, layout.meta_path(rel), "an index column is missing")?;
    let mut entries = Vec::with_capacity(usize::try_from(run.len()).unwrap_or(0));
    column.scan(run.from, run.to, |first, keys| {
        entries.extend(keys.iter().zip(first..).map(|(key, rec)| (*key, rec)));
        ControlFlow::<()>::Continue(())
    })?;
    if bytes::to_u64(entries.len()) != run.len() {
        return Err(StoreError::IndexCorrupt {
            what: "an index column is shorter than the checkpoint passed",
        });
    }
    Ok(entries)
}

/// Removes the run files `manifest` does not name and every file of `tmp/` (rule 7): under
/// `views.lock` no other step writes them.
fn sweep(layout: &Layout, fs: &StoreFs, manifest: &Manifest) -> Result<(), StoreError> {
    let dir = layout.views_dir();
    let live: Vec<String> = manifest.runs.iter().map(Run::name).collect();
    let mut removed = false;
    for name in names(fs, &dir)? {
        if runs::is_run_name(&name) && !live.contains(&name) {
            remove(fs, &dir.join(&name))?;
            removed = true;
        }
    }
    let tmp = dir.join(runs::TMP);
    for name in names(fs, &tmp)? {
        remove(fs, &tmp.join(&name))?;
    }
    if removed {
        fs.fsync_dir(dir.as_std_path())?;
    }
    Ok(())
}

/// The names in directory `dir`; none if it is missing.
fn names(fs: &StoreFs, dir: &camino::Utf8Path) -> Result<Vec<String>, StoreError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(fs.locate(FileOp::ReadDir, dir, e).into()),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| fs.locate(FileOp::ReadDir, dir, e))?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    Ok(names)
}

/// Removes the file at `path` if it is there.
fn remove(fs: &StoreFs, path: &camino::Utf8Path) -> Result<(), StoreError> {
    match fs.remove(path.as_std_path()) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// `meta/views.lock`, held, if no other step holds it: opened as `meta/lock` is (made if
/// missing), flocked without waiting.
fn try_lease(layout: &Layout, fs: &StoreFs) -> Result<Option<File>, StoreError> {
    let path = layout.views_lock();
    let file = open_flock_file(path.as_std_path(), Create::IfMissing)
        .map_err(|e| fs.locate(FileOp::Open, &path, e))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(fs.locate(FileOp::Lock, &path, e).into()),
    }
}
