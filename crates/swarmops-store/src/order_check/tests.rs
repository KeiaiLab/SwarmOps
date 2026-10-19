//! Each rule, broken on purpose by a call sequence the store never makes, panics with its
//! id; kept, the same steps pass. The stores' own sweeps run under the checker elsewhere.

use std::panic::{self, AssertUnwindSafe};

use swarmops_core::{ChangesetId, ObjectId};

use super::*;
use crate::fs_ops::{FailAt, FailAtFs, FailMode, InjectedError, RealFs};
use crate::meta::frame::encode_frame;

/// A store root in a temp dir, with `meta/`, empty journal files, `meta/refs/` and an
/// `objects/` fan-out directory, all durable.
fn root() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    for sub in [
        "meta/refs",
        "meta/idx",
        "meta/sorted",
        "objects/ab",
        "objects/tmp",
    ] {
        fs::create_dir_all(root.join(sub)).unwrap();
    }
    for file in ["meta/journal.log", "meta/journal.off"] {
        File::create(root.join(file)).unwrap();
    }
    (dir, root)
}

fn frame(records: &[Record]) -> Vec<u8> {
    encode_frame(records).unwrap()
}

fn compacted() -> Vec<u8> {
    frame(&[Record::Compacted { sorted_count: 0 }])
}

fn changeset() -> Vec<u8> {
    frame(&[Record::Changeset {
        id: ChangesetId(ObjectId([1; 32])),
        generation: 1,
        delta_hash: [2; 32],
        parents: Vec::new(),
    }])
}

/// The message `run` panics with; `None` if it does not panic.
fn panic_message(run: impl FnOnce()) -> Option<String> {
    let payload = panic::catch_unwind(AssertUnwindSafe(run)).err()?;
    Some(
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_default(),
    )
}

/// The rule `run` breaks, from its panic; `None` if it does not panic.
fn broken(run: impl FnOnce()) -> Option<String> {
    let message = panic_message(run)?;
    let rule = message.strip_prefix("order check: ")?.split(' ').next()?;
    Some(rule.to_owned())
}

#[track_caller]
fn assert_breaks(rule: &str, run: impl FnOnce()) {
    assert_eq!(broken(run).as_deref(), Some(rule));
}

/// The rule ids are W1 to W7, the names storage spec §2 rule 4 states the rules under, and
/// a violation's panic starts with its rule's id.
#[test]
fn rule_ids_are_w1_to_w7() {
    let ids: Vec<String> = Rule::ALL.iter().map(ToString::to_string).collect();
    assert_eq!(ids, ["W1", "W2", "W3", "W4", "W5", "W6", "W7"]);
    let off = Path::new("meta/journal.off");
    for (n, rule) in (1..).zip(Rule::ALL) {
        let message = panic_message(|| {
            verdict(
                rule,
                FsMethod::AppendDurable,
                &[off],
                Some("why".to_owned()),
            );
        });
        let expected = format!(
            "order check: W{n} violated by append_durable {}: why",
            off.display()
        );
        assert_eq!(message.as_deref(), Some(expected.as_str()));
    }
}

/// Storage spec §2 rule 4 states every rule the checker enforces, under its id, and the
/// checker's old ids are gone from the spec: "R1" is only §6's first review invariant.
#[test]
fn the_spec_states_every_write_order_rule() {
    const SPEC: &str = include_str!(
        "../../../../docs/superpowers/specs/2026-10-15-swarmops-storage-core-design.md"
    );
    let section = |n: u32| {
        let start = SPEC
            .find(&format!("\n## {n}. "))
            .expect("the section's heading");
        let len = SPEC[start + 1..]
            .find("\n## ")
            .map_or(SPEC.len() - start, |end| end + 1);
        &SPEC[start..start + len]
    };
    let disk = section(2);
    for rule in Rule::ALL {
        assert!(
            disk.contains(&format!("- {rule} ")),
            "storage spec §2 does not state {rule}"
        );
    }
    let review = section(6);
    assert_eq!(
        SPEC.matches("R1").count(),
        review.matches("R1").count(),
        "storage spec names R1 outside §6"
    );
}

/// `RealFs` whose `k`-th call fails with `EIO` once: a flushing call does its work first.
fn eio_at(k: u64) -> FailAtFs<RealFs> {
    let fail = FailAt {
        at: k,
        mode: FailMode::Once,
        error: InjectedError::Eio,
    };
    FailAtFs::new(RealFs, fail)
}

#[test]
fn w1_a_frame_is_listed_only_once_it_is_durable() {
    let (_dir, root) = root();
    let (log, off) = (root.join("meta/journal.log"), root.join("meta/journal.off"));
    let fs = OrderCheckFs::new(RealFs);
    // Written, never synced: listing it breaks W1.
    let at = fs.append(&log, &compacted()).unwrap();
    assert_breaks("W1", || {
        fs.append_durable(&off, &at.to_le_bytes()).unwrap();
    });
    // Synced first, it may be listed.
    let fs = OrderCheckFs::new(RealFs);
    fs.sync_data(&log).unwrap();
    fs.append_durable(&off, &at.to_le_bytes()).unwrap();
    let at = fs.append_durable(&log, &compacted()).unwrap();
    fs.append_durable(&off, &at.to_le_bytes()).unwrap();
}

/// The fsyncgate pattern: a frame whose sync failed, then listed after a later sync that
/// succeeds, breaks W1 (Linux reports a failed writeback once); and so does any append to a
/// journal file after bytes a failed sync left behind, until a cut takes them off.
#[test]
fn w1_a_failed_sync_is_not_made_good_by_a_later_one() {
    let (_dir, root) = root();
    let (log, off) = (root.join("meta/journal.log"), root.join("meta/journal.off"));
    let fs = OrderCheckFs::new(eio_at(1));
    let start: u64 = 0;
    fs.append_durable(&log, &compacted()).unwrap_err();
    fs.sync_data(&log).unwrap();
    assert_breaks("W1", || {
        fs.append_durable(&off, &start.to_le_bytes()).unwrap();
    });
    assert_breaks("W1", || {
        fs.append_durable(&log, &compacted()).unwrap();
    });
    // Cut back to where the frame started, the journal takes appends again.
    fs.truncate(&log, start).unwrap();
    let at = fs.append_durable(&log, &compacted()).unwrap();
    fs.append_durable(&off, &at.to_le_bytes()).unwrap();

    // An entry whose sync failed: the next entry would follow it.
    let fs = OrderCheckFs::new(eio_at(2));
    let at = fs.append_durable(&log, &compacted()).unwrap();
    fs.append_durable(&off, &at.to_le_bytes()).unwrap_err();
    let next = fs.append_durable(&log, &compacted()).unwrap();
    assert_breaks("W1", || {
        fs.append_durable(&off, &next.to_le_bytes()).unwrap();
    });
}

/// A cut never takes what the journal vouches for: of `journal.log`, a frame `journal.off`
/// lists; of `journal.off`, an entry this process synced, or found there when it first saw the
/// file (entry 0 lists frame 0 at offset 0, whatever it holds). A torn tail past the listed
/// frames may go, and so may a partial entry, and the zeroed entries past frame 0 that a power
/// loss leaves at the end of `journal.off`, which list no frame (the tail check drops them).
#[test]
fn w1_a_cut_never_takes_a_listed_frame_or_a_synced_entry() {
    let (_dir, root) = root();
    let (log, off) = (root.join("meta/journal.log"), root.join("meta/journal.off"));
    let fs = OrderCheckFs::new(RealFs);
    let at = fs.append_durable(&log, &compacted()).unwrap();
    fs.append_durable(&off, &at.to_le_bytes()).unwrap();
    let end = at + to_u64(compacted().len());
    assert_breaks("W1", || fs.truncate(&log, end - 1).unwrap());
    assert_breaks("W1", || fs.truncate(&off, 4).unwrap());
    fs.append(&log, b"torn").unwrap();
    fs.truncate(&log, end).unwrap();
    fs.append(&off, &[1, 2, 3]).unwrap();
    fs.truncate(&off, ENTRY_LEN).unwrap();

    // What a crash left, as a new process finds it: two listed frames, then a zeroed entry.
    let frames = [compacted(), compacted()].concat();
    fs::write(&log, &frames).unwrap();
    let second = to_u64(compacted().len());
    let entries = [0, second, 0].map(u64::to_le_bytes).concat();
    fs::write(&off, &entries).unwrap();
    let fs = OrderCheckFs::new(RealFs);
    assert_breaks("W1", || fs.truncate(&off, ENTRY_LEN).unwrap());
    assert_breaks("W1", || fs.truncate(&log, second).unwrap());
    fs.truncate(&off, 2 * ENTRY_LEN).unwrap();
    fs.truncate(&log, to_u64(frames.len())).unwrap();
}

#[test]
fn w2_a_ref_moves_only_once_its_frame_is_durable() {
    let (_dir, root) = root();
    let (log, main) = (root.join("meta/journal.log"), root.join("meta/refs/main"));
    let fs = OrderCheckFs::new(RealFs);
    fs.append(&log, &compacted()).unwrap();
    assert_breaks("W2", || fs.atomic_replace(&main, b"x").unwrap());
    fs.sync_data(&log).unwrap();
    fs.atomic_replace(&main, b"x").unwrap();
    fs.append(&log, &compacted()).unwrap();
    assert_breaks("W2", || fs.remove(&main).unwrap());
}

#[test]
fn w3_the_checkpoint_waits_for_the_index_the_journal_and_the_refs() {
    let (_dir, root) = root();
    let meta = root.join("meta");
    let (log, ckpt) = (meta.join("journal.log"), meta.join("journal.ckpt"));
    let fs = OrderCheckFs::new(RealFs);
    let end = fs.append_durable(&log, &compacted()).unwrap() + to_u64(compacted().len());
    for path in index_files(&meta) {
        fs.append(&path, &[0; 8]).unwrap();
    }
    let ckpt_at = |offset: u64| {
        fs.atomic_replace(&ckpt, &offset.to_le_bytes()).unwrap();
    };
    assert_breaks("W3", || ckpt_at(end));
    ckpt_at(0); // covers nothing
    for path in index_files(&meta) {
        fs.sync_data(&path).unwrap();
    }
    assert_breaks("W3", || ckpt_at(end + 1));
    ckpt_at(end);
    // A ref file's removal not yet made durable.
    let old = meta.join("refs/old");
    fs.atomic_replace(&old, b"x").unwrap();
    fs.remove(&old).unwrap();
    assert_breaks("W3", || ckpt_at(end));
    fs.fsync_dir(&meta.join("refs")).unwrap();
    ckpt_at(end);
    // Synced, then written again: not synced since it last changed.
    fs.append(&meta.join("idx/ids.col"), &[0; 32]).unwrap();
    assert_breaks("W3", || ckpt_at(end));
    fs.sync_data(&meta.join("idx/ids.col")).unwrap();
    ckpt_at(end);
    // A failed index sync leaves records not durable, whatever later syncs say.
    let fs = OrderCheckFs::new(eio_at(2));
    let ids = meta.join("idx/ids.col");
    fs.append(&ids, &[0; 32]).unwrap();
    fs.sync_data(&ids).unwrap_err();
    for path in index_files(&meta) {
        fs.sync_data(&path).unwrap();
    }
    let checkpoint = || {
        fs.atomic_replace(&ckpt, &end.to_le_bytes()).unwrap();
    };
    assert_breaks("W3", checkpoint);
}

/// The view files of `meta`, `entries` each, written through `fs` to temp files and synced.
fn temp_views(fs: &dyn FsOps, meta: &Path, entries: usize) {
    for (view, width) in VIEWS {
        let temp = meta.join(format!("sorted/{view}.tmp"));
        fs.append(&temp, &vec![0; entries * to_usize(width)])
            .unwrap();
        fs.sync_data(&temp).unwrap();
    }
}

/// A store root with one index record in every index file, synced unless `synced` is
/// false, the view temp files of one entry each written and synced, and what renames a
/// view's temp file into place.
fn compacting(
    synced: bool,
) -> (
    tempfile::TempDir,
    PathBuf,
    OrderCheckFs<RealFs>,
    impl Fn(&OrderCheckFs<RealFs>, &str),
) {
    let (dir, root) = root();
    let meta = root.join("meta");
    let fs = OrderCheckFs::new(RealFs);
    for path in index_files(&meta) {
        fs.append(&path, &[0; 32]).unwrap();
        if synced {
            fs.sync_data(&path).unwrap();
        }
    }
    temp_views(&fs, &meta, 1);
    let sorted = meta.join("sorted");
    let rename = move |fs: &OrderCheckFs<RealFs>, view: &str| {
        let (temp, to) = (sorted.join(format!("{view}.tmp")), sorted.join(view));
        fs.rename_durable(&temp, &to).unwrap();
    };
    (dir, root, fs, rename)
}

#[test]
fn w4_views_are_renamed_synced_after_the_index_and_in_order() {
    let (_dir, _root, fs, rename) = compacting(false);
    assert_breaks("W4", || rename(&fs, "ids.key")); // the index is not synced
    let (_dir, _root, fs, rename) = compacting(true);
    assert_breaks("W4", || rename(&fs, "dh.key")); // the counts grow along the order
    let (_dir, root, fs, rename) = compacting(true);
    for view in ["ids.key", "ids.rec", "dh.key", "dh.rec"] {
        rename(&fs, view);
    }
    // A temp file written since its sync breaks W6.
    temp_views(&fs, &root.join("meta"), 2);
    fs.append(&root.join("meta/sorted/ids.key.tmp"), &[0; 32])
        .unwrap();
    assert_breaks("W6", || rename(&fs, "ids.key"));
    // The index synced, then written again: not synced since it last changed (W4), even
    // with the temp synced (W6 kept).
    fs.sync_data(&root.join("meta/sorted/ids.key.tmp")).unwrap();
    fs.append(&root.join("meta/idx/ids.col"), &[0; 32]).unwrap();
    assert_breaks("W4", || rename(&fs, "ids.key"));
    fs.sync_data(&root.join("meta/idx/ids.col")).unwrap();
    rename(&fs, "ids.key");
}

#[test]
fn w5_objects_are_durable_before_the_frame_that_names_them() {
    let (_dir, root) = root();
    let (log, staging) = (root.join("meta/journal.log"), root.join("objects/tmp"));
    let object = root.join("objects/ab/cdef");
    let fs = OrderCheckFs::new(RealFs);
    let unsynced = staging.join("unsynced");
    fs.append(&unsynced, b"object").unwrap();
    assert_breaks("W5", || {
        fs.link_noclobber(&unsynced, &object).unwrap();
    });
    let temp = fs.write_durable_temp(&staging, b"object").unwrap();
    fs.link_noclobber(&temp, &object).unwrap();
    fs.append_durable(&log, &compacted()).unwrap(); // records no changeset
    assert_breaks("W5", || {
        fs.append_durable(&log, &changeset()).unwrap();
    });
    fs.fsync_dir(&root.join("objects/ab")).unwrap();
    fs.append_durable(&log, &changeset()).unwrap();
}

#[test]
fn w6_a_directory_is_renamed_only_once_its_tree_is_durable() {
    let (_dir, root) = root();
    let staged = root.join("meta.tmp/meta");
    let fs = OrderCheckFs::new(RealFs);
    fs.create_dir_all(&staged.join("idx")).unwrap();
    fs.append(&staged.join("idx/ids.col"), &[0; 8]).unwrap();
    fs.sync_data(&staged.join("idx/ids.col")).unwrap();
    let to = root.join("staged");
    assert_breaks("W6", || fs.rename_durable(&staged, &to).unwrap());
    fs.fsync_dir(&staged).unwrap(); // `idx` is durable, the entry `ids.col` in it is not
    assert_breaks("W6", || fs.rename_durable(&staged, &to).unwrap());
    fs.fsync_dir(&staged.join("idx")).unwrap();
    // Every entry durable, the file written since its sync.
    fs.append(&staged.join("idx/ids.col"), &[0; 8]).unwrap();
    assert_breaks("W6", || fs.rename_durable(&staged, &to).unwrap());
    fs.sync_data(&staged.join("idx/ids.col")).unwrap();
    fs.rename_durable(&staged, &to).unwrap();
}

#[test]
fn w7_format_is_linked_last() {
    let (_dir, root) = root();
    let fs = OrderCheckFs::new(RealFs);
    fs.create_dir_all(&root.join("objects/cd")).unwrap();
    let format = root.join("FORMAT");
    let link = |fs: &OrderCheckFs<RealFs>| {
        let temp = fs
            .write_durable_temp(&root.join("objects/tmp"), b"1\n")
            .unwrap();
        fs.link_noclobber(&temp, &format).unwrap();
    };
    assert_breaks("W7", || link(&fs));
    fs.fsync_dir(&root.join("objects")).unwrap();
    // The rest of the store durable, FORMAT's own data must be synced too, though the staging
    // directory it is written in is exempt from the rest.
    let unsynced = root.join("objects/tmp/format");
    fs.append(&unsynced, b"1\n").unwrap();
    assert_breaks("W7", || {
        fs.link_noclobber(&unsynced, &format).unwrap();
    });
    link(&fs);
}

fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap()
}

fn to_usize(n: u64) -> usize {
    usize::try_from(n).unwrap()
}
