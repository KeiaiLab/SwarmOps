//! Each rule, broken on purpose by a call sequence the store never makes, panics with its
//! id; kept, the same steps pass. The stores' own sweeps run under the checker elsewhere.

use std::panic::{self, AssertUnwindSafe};

use swarmops_core::{ChangesetId, ObjectId};

use super::*;
use crate::fs_ops::{FailAt, FailAtFs, FailMode, InjectedError, RealFs};
use crate::meta::frame::{Fact, FactKind};
use crate::meta::journal::Ckpt3;
use crate::meta::runs::{self, Domain, Manifest, Run};

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
    frame::encode_frame(records).unwrap()
}

fn compacted() -> Vec<u8> {
    frame(&[Record::Compacted { sorted_count: 0 }])
}

fn changeset() -> Vec<u8> {
    changeset_of([1; 32])
}

/// A frame recording the root changeset `id`.
fn changeset_of(id: [u8; 32]) -> Vec<u8> {
    frame(&[Record::Changeset {
        id: ChangesetId::new(ObjectId::new(id)),
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

/// [`assert_breaks`], and the panic names `method` as the call that broke the rule.
#[track_caller]
fn assert_breaks_by(rule: &str, method: &str, run: impl FnOnce()) {
    let message = panic_message(run).unwrap_or_default();
    let by = format!("order check: {rule} violated by {method} ");
    assert!(message.starts_with(&by), "{message:?} is not {by:?}");
}

/// The rule ids are W1 to W11, the names storage spec §2 rule 4 and the FORMAT 3 design §2.9
/// state the rules under, and a violation's panic starts with its rule's id.
#[test]
fn rule_ids_are_w1_to_w11() {
    let ids: Vec<String> = Rule::ALL.iter().map(ToString::to_string).collect();
    let expected = [
        "W1", "W2", "W3", "W4", "W5", "W6", "W7", "W8", "W9", "W10", "W11",
    ];
    assert_eq!(ids, expected);
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

/// Section `n` of storage spec `spec`, from its heading to the next.
fn section(spec: &str, n: u32) -> &str {
    let start = spec
        .find(&format!("\n## {n}. "))
        .expect("the section's heading");
    let len = spec[start + 1..]
        .find("\n## ")
        .map_or(spec.len() - start, |end| end + 1);
    &spec[start..start + len]
}

/// The document that states a rule.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Doc {
    /// Storage spec §2 rule 4: W1 to W8.
    Storage,
    /// The FORMAT 3 design §2.9: the rules FORMAT 3 adds.
    Format3,
}

/// Where `rule` is stated.
fn stated_in(rule: Rule) -> Doc {
    match rule {
        Rule::W9 | Rule::W10 | Rule::W11 => Doc::Format3,
        _ => Doc::Storage,
    }
}

/// The rules the checker enforces that §2 of storage spec `spec` does not state under their ids.
fn unstated(spec: &str) -> Vec<Rule> {
    let disk = section(spec, 2);
    (Rule::ALL.into_iter())
        .filter(|&rule| stated_in(rule) == Doc::Storage)
        .filter(|rule| !disk.contains(&format!("- {rule} ")))
        .collect()
}

/// The rules FORMAT 3 adds that §2.9 of the FORMAT 3 design `design` does not state under their
/// ids, each as a bullet `- **W<n> `.
fn unstated_in_format3(design: &str) -> Vec<Rule> {
    let start = design.find("\n### 2.9 ").expect("the section's heading");
    let len = design[start + 1..]
        .find("\n#")
        .map_or(design.len() - start, |end| end + 1);
    let rules = &design[start..start + len];
    (Rule::ALL.into_iter())
        .filter(|&rule| stated_in(rule) == Doc::Format3)
        .filter(|rule| !rules.contains(&format!("- **{rule} ")))
        .collect()
}

/// The checker's old ids, R1 to R7, that storage spec `spec` names outside §6, whose review
/// invariants R1 to R4 are: each counted where it stands as a word of its own (not in `R10`).
fn old_ids_outside_the_reviews(spec: &str) -> Vec<String> {
    let words = |text: &str, id: &str| {
        let alone = |c: Option<char>| !c.is_some_and(|c| c.is_ascii_alphanumeric());
        (text.match_indices(id))
            .filter(|&(at, _)| {
                alone(text[..at].chars().next_back()) && alone(text[at + id.len()..].chars().next())
            })
            .count()
    };
    let review = section(spec, 6);
    (1..=7)
        .map(|n| format!("R{n}"))
        .filter(|id| words(spec, id) != words(review, id))
        .collect()
}

/// The variable that names the directory of the maintainers' private design documents (its
/// `specs/` and `plans/`), which the test of the storage spec's text reads.
const PRIVATE_DOCS: &str = "SWARMOPS_PRIVATE_DOCS";

/// Storage spec §2 rule 4 states every rule the checker enforces, under its id, but those
/// FORMAT 3 adds, which the FORMAT 3 design §2.9 states; and the checker's old ids, R1 to R7,
/// are gone from the spec: R1 to R4 are only §6's review invariants. A document that leaves a
/// rule out, or names an old id beside its rule, fails. Both are the maintainers' private design
/// documents, read from the directory `SWARMOPS_PRIVATE_DOCS` names; the test is skipped, saying
/// so, when it is unset.
#[test]
fn the_spec_states_every_write_order_rule() {
    let Some(docs) = std::env::var_os(PRIVATE_DOCS).filter(|docs| !docs.is_empty()) else {
        eprintln!("the_spec_states_every_write_order_rule: {PRIVATE_DOCS} is not set; skipped");
        return;
    };
    let path = PathBuf::from(&docs).join("specs/2026-10-15-swarmops-storage-core-design.md");
    let spec = fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    assert_eq!(
        unstated(&spec),
        [],
        "storage spec §2 does not state these rules"
    );
    assert_eq!(
        old_ids_outside_the_reviews(&spec),
        Vec::<String>::new(),
        "storage spec names these old ids outside §6"
    );
    assert_eq!(unstated(&spec.replacen("- W3 ", "- ", 1)), [Rule::W3]);
    let stale = spec.replacen("- W7 ", "- W7 (옛 R7) ", 1);
    assert_eq!(old_ids_outside_the_reviews(&stale), ["R7"]);
    let stale = spec.replacen("- W1 ", "- W1 (R1) ", 1);
    assert_eq!(old_ids_outside_the_reviews(&stale), ["R1"]);

    // The rules FORMAT 3 adds, in its design; one left out is caught.
    let path = PathBuf::from(docs).join("specs/2026-11-07-format3-design.md");
    let design =
        fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    assert_eq!(
        unstated_in_format3(&design),
        [],
        "the FORMAT 3 design §2.9 does not state these rules"
    );
    let planted = design.replacen("- **W10 ", "- **", 1);
    assert_eq!(unstated_in_format3(&planted), [Rule::W10]);
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
    assert_breaks_by("W1", "append_durable", || {
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
    assert_breaks_by("W1", "truncate", || fs.truncate(&log, end - 1).unwrap());
    assert_breaks("W1", || fs.truncate(&off, 4).unwrap());
    assert_breaks("W1", || fs.truncate(&off, ENTRY_LEN - 1).unwrap());
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
    assert_breaks("W1", || fs.truncate(&off, 2 * ENTRY_LEN - 1).unwrap());
    assert_breaks("W1", || fs.truncate(&log, second).unwrap());
    fs.truncate(&off, 2 * ENTRY_LEN).unwrap();
    fs.truncate(&log, to_u64(frames.len())).unwrap();
}

/// An entry of `journal.off` that the checker cannot read counts as one that lists a frame (the
/// safe way): a `journal.off` that holds no entry it can read, a directory in its place here, is
/// not cut below the entries this process synced there.
#[test]
fn w1_an_entry_the_checker_cannot_read_counts_as_listed() {
    let (_dir, root) = root();
    let (log, off) = (root.join("meta/journal.log"), root.join("meta/journal.off"));
    let fs = OrderCheckFs::new(RealFs);
    let first = fs.append_durable(&log, &compacted()).unwrap();
    fs.append_durable(&off, &first.to_le_bytes()).unwrap();
    let second = fs.append_durable(&log, &compacted()).unwrap();
    fs.append_durable(&off, &second.to_le_bytes()).unwrap();
    fs::remove_file(&off).unwrap();
    fs::create_dir(&off).unwrap();
    // A long name, so that the directory's length covers both entries on any filesystem.
    File::create(off.join("a-name-of-thirty-two-characters.")).unwrap();
    assert!(real_len(&off) >= 2 * ENTRY_LEN, "{}", real_len(&off));
    assert_breaks("W1", || fs.truncate(&off, ENTRY_LEN).unwrap());
}

#[test]
fn w2_a_ref_moves_only_once_its_frame_is_durable() {
    let (_dir, root) = root();
    let (log, main) = (root.join("meta/journal.log"), root.join("meta/refs/main"));
    let fs = OrderCheckFs::new(RealFs);
    fs.append(&log, &compacted()).unwrap();
    assert_breaks_by("W2", "atomic_replace", || {
        fs.atomic_replace(&main, b"x").unwrap();
    });
    fs.sync_data(&log).unwrap();
    fs.atomic_replace(&main, b"x").unwrap();
    fs.append(&log, &compacted()).unwrap();
    assert_breaks_by("W2", "remove", || fs.remove(&main).unwrap());
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
    // The index is not synced: refused before the rename.
    assert_breaks_by("W4", "rename_durable", || rename(&fs, "ids.key"));
    let (_dir, _root, fs, rename) = compacting(true);
    // The counts grow along the order: refused once the rename is done.
    assert_breaks_by("W4", "rename_durable", || rename(&fs, "dh.key"));
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
    assert_breaks_by("W5", "link_noclobber", || {
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

/// An append of several frames, a batch, is judged frame by frame: a changeset recorded in its
/// second frame, behind a `Compacted` one, breaks W5 while an object is not durable, as it would
/// alone; once the object is, the same append passes.
#[test]
fn w5_is_judged_on_every_frame_of_an_append() {
    let (_dir, root) = root();
    let log = root.join("meta/journal.log");
    let fs = OrderCheckFs::new(RealFs);
    let temp = fs
        .write_durable_temp(&root.join("objects/tmp"), b"object")
        .unwrap();
    fs.link_noclobber(&temp, &root.join("objects/ab/cdef"))
        .unwrap();
    let batch = [compacted(), changeset()].concat();
    fs.append_durable(&log, &[compacted(), compacted()].concat())
        .unwrap(); // records no changeset
    assert_breaks("W5", || {
        fs.append_durable(&log, &batch).unwrap();
    });
    fs.fsync_dir(&root.join("objects/ab")).unwrap();
    fs.append_durable(&log, &batch).unwrap();
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

/// A file renamed into another directory moves only once its own entry is durable: the rename
/// syncs the target's directory alone, and a power loss may take back a new name in the source
/// directory, and the file with it.
#[test]
fn w6_a_file_moved_out_of_its_directory_has_a_durable_entry() {
    let (_dir, root) = root3();
    let (tmp, views) = (root.join("meta/sorted3/tmp"), root.join("meta/sorted3"));
    let fs = OrderCheckFs::new(RealFs);
    let made = tmp.join("made");
    fs.append(&made, b"data").unwrap();
    fs.sync_data(&made).unwrap();
    assert_breaks_by("W6", "rename_durable", || {
        fs.rename_durable(&made, &views.join("moved")).unwrap();
    });

    // Its entry durable: it moves. Renamed within its directory, the target's sync covers it.
    fs.fsync_dir(&tmp).unwrap();
    fs.rename_durable(&made, &views.join("moved")).unwrap();
    let again = views.join("again");
    fs.append(&again, b"data").unwrap();
    fs.sync_data(&again).unwrap();
    fs.rename_durable(&again, &views.join("kept")).unwrap();
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

/// The 44 bytes of an external id entry naming changeset `id`, its CRC left 0: the checker reads
/// the id, bytes 8..40, and nothing else.
fn entry(id: [u8; 32]) -> Vec<u8> {
    let mut bytes = b"SXID\x01\x00\x00\x00".to_vec();
    bytes.extend_from_slice(&id);
    bytes.extend_from_slice(&[0; 4]);
    bytes
}

/// An external id entry is linked only from synced data, and only once a frame that records the
/// entry's changeset (bytes 8..40 of the entry) is durable in `journal.log`: synced in this
/// process, or listed, by whichever process wrote it.
#[test]
fn w8_an_external_id_is_linked_only_once_its_frame_is_durable() {
    use std::io::Write as _;

    let (_dir, root) = root();
    let (log, off) = (root.join("meta/journal.log"), root.join("meta/journal.off"));
    let staging = root.join("objects/tmp");
    let fanout = root.join("meta/external_ids/git/ab");
    fs::create_dir_all(&fanout).unwrap();
    let fs = OrderCheckFs::new(RealFs);

    // 1. The changeset's frame written, not synced: the entry waits.
    let temp = fs.write_durable_temp(&staging, &entry([1; 32])).unwrap();
    fs.append(&log, &changeset()).unwrap();
    assert_breaks_by("W8", "link_noclobber", || {
        fs.link_noclobber(&temp, &fanout.join("cdef")).unwrap();
    });
    fs.sync_data(&log).unwrap();
    fs.link_noclobber(&temp, &fanout.join("cdef")).unwrap();

    // 2. A temp file written but not synced breaks it too.
    let unsynced = staging.join("unsynced");
    fs.append(&unsynced, &entry([1; 32])).unwrap();
    assert_breaks("W8", || {
        fs.link_noclobber(&unsynced, &fanout.join("0123")).unwrap();
    });

    // 3. Every byte of journal.log durable, but no frame records the entry's changeset; nor
    //    does a temp file that holds no entry name one.
    let stray = fs.write_durable_temp(&staging, &entry([9; 32])).unwrap();
    assert_breaks("W8", || {
        fs.link_noclobber(&stray, &fanout.join("4567")).unwrap();
    });
    let short = fs.write_durable_temp(&staging, b"entry").unwrap();
    assert_breaks("W8", || {
        fs.link_noclobber(&short, &fanout.join("4567")).unwrap();
    });

    // 4. Another process's frame, written and not synced yet, holds the entry back; listed by
    //    that process, the frame is durable, and the entry may follow in any process.
    let at = real_len(&log);
    let mut other_log = fs::OpenOptions::new().append(true).open(&log).unwrap();
    other_log.write_all(&changeset_of([3; 32])).unwrap();
    let temp = fs.write_durable_temp(&staging, &entry([3; 32])).unwrap();
    assert_breaks("W8", || {
        fs.link_noclobber(&temp, &fanout.join("89ab")).unwrap();
    });
    let mut other_off = fs::OpenOptions::new().append(true).open(&off).unwrap();
    for start in [0, at] {
        other_off.write_all(&start.to_le_bytes()).unwrap();
    }
    fs.link_noclobber(&temp, &fanout.join("89ab")).unwrap();
    let other = OrderCheckFs::new(RealFs);
    let temp = other.write_durable_temp(&staging, &entry([3; 32])).unwrap();
    other.link_noclobber(&temp, &fanout.join("cdab")).unwrap();
}

/// A frame whose sync failed holds its changeset's entries back for good: Linux reports a failed
/// writeback once, so a later sync that succeeds does not make the frame durable.
#[test]
fn w8_a_frame_a_failed_sync_left_holds_its_entry_back() {
    let (_dir, root) = root();
    let log = root.join("meta/journal.log");
    let staging = root.join("objects/tmp");
    let fanout = root.join("meta/external_ids/git/ab");
    fs::create_dir_all(&fanout).unwrap();
    let fs = OrderCheckFs::new(eio_at(1));
    fs.append_durable(&log, &changeset()).unwrap_err();
    fs.sync_data(&log).unwrap();
    let temp = fs.write_durable_temp(&staging, &entry([1; 32])).unwrap();
    assert_breaks("W8", || {
        fs.link_noclobber(&temp, &fanout.join("cdef")).unwrap();
    });
}

/// A FORMAT 3 store root in a temp dir: `FORMAT`, `meta/` with empty `journal2.log` and
/// `journal2.off`, `meta/refs2/`, `meta/idx/`, `meta/sorted3/tmp/` and an `objects/` fan-out
/// directory, all made outside the checker, so durable.
fn root3() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    for sub in [
        "meta/refs2",
        "meta/idx",
        "meta/sorted3/tmp",
        "objects/ab",
        "objects/tmp",
    ] {
        fs::create_dir_all(root.join(sub)).unwrap();
    }
    for file in ["meta/journal2.log", "meta/journal2.off"] {
        File::create(root.join(file)).unwrap();
    }
    fs::write(root.join("FORMAT"), StoreFormat::V3.marker()).unwrap();
    (dir, root)
}

/// The FORMAT 3 frame `seq` (the first of its journal, so after no CRC) of `records`, ending in
/// a nonce.
fn frame3(records: &[Record], seq: u64) -> Vec<u8> {
    frame::encode_frame_with_nonce(StoreFormat::V3, records, Some(7), seq, 0).unwrap()
}

/// The FORMAT 3 checkpoint at `offset`, passing `records` changeset records and no fact row.
fn ckpt3(offset: u64, records: u64) -> Vec<u8> {
    let ckpt = Ckpt3 {
        offset,
        records,
        ..Ckpt3::default()
    };
    ckpt.encode()
}

/// The FORMAT 3 index files of `meta`: the five, then `facts.col`.
fn index_files3(meta: &Path) -> Vec<PathBuf> {
    let mut files = index_files(meta).to_vec();
    files.push(meta.join("idx/facts.col"));
    files
}

/// An annotation that names blob `[4; 32]` as its value.
fn annotation() -> Record {
    let body = [&[1, b'k'][..], &[4; 32]].concat();
    let fact = Fact::new(
        FactKind::Annotation,
        0,
        [8; 32],
        [0; 32],
        0,
        String::new(),
        body,
    );
    Record::Fact(fact.unwrap())
}

/// W3's FORMAT 3 form: `journal3.ckpt` moves past 0 only once the six index files, `facts.col`
/// among them, are synced since they last changed; it holds 44 bytes, and no poison.
#[test]
fn w3_a_format_3_checkpoint_waits_for_six_index_files() {
    let (_dir, root) = root3();
    let meta = root.join("meta");
    let (log, ckpt) = (meta.join("journal2.log"), meta.join("journal3.ckpt"));
    let fs = OrderCheckFs::new(RealFs);
    let rename = Record::RefMove {
        name: "main".to_owned(),
        old: None,
        new: None,
        actor: String::new(),
        ts: 0,
    };
    let frame = frame3(&[rename], 0);
    let end = fs.append_durable(&log, &frame).unwrap() + to_u64(frame.len());
    for path in index_files3(&meta) {
        fs.append(&path, &[0; 8]).unwrap();
    }

    // 1. Five synced, facts.col not: refused.
    for path in index_files(&meta) {
        fs.sync_data(&path).unwrap();
    }
    assert_breaks_by("W3", "atomic_replace", || {
        fs.atomic_replace(&ckpt, &ckpt3(end, 0)).unwrap();
    });

    // 2. A checkpoint at 0 covers nothing; one of another length, or the poison, is no
    //    FORMAT 3 checkpoint.
    fs.atomic_replace(&ckpt, &ckpt3(0, 0)).unwrap();
    assert_breaks("W3", || fs.atomic_replace(&ckpt, &[0; 20]).unwrap());
    assert_breaks("W3", || {
        fs.atomic_replace(&ckpt, UPGRADE_POISON_V3).unwrap();
    });

    // 3. All six synced: it moves, but never past the durable journal.
    fs.sync_data(&meta.join("idx/facts.col")).unwrap();
    assert_breaks("W3", || {
        fs.atomic_replace(&ckpt, &ckpt3(end + 1, 0)).unwrap();
    });
    fs.atomic_replace(&ckpt, &ckpt3(end, 0)).unwrap();
}

/// A run file of `meta`'s `sorted3/` of the changeset records `[from, to)`, its sections empty:
/// the checker reads its header only.
fn run_bytes(from: u64, to: u64) -> (Run, Vec<u8>) {
    let run = Run {
        domain: Domain::Changesets,
        from,
        to,
    };
    let bytes = runs::encode_run(&run, [&[], &[]]);
    (run, bytes)
}

/// Writes `run`'s file in `sorted3/tmp/` through `fs` and makes it durable, its entry too;
/// returns its path there.
fn temp_run(fs: &OrderCheckFs<RealFs>, meta: &Path, run: &Run, bytes: &[u8]) -> PathBuf {
    let tmp = meta.join("sorted3/tmp").join(run.name());
    fs.append(&tmp, bytes).unwrap();
    fs.sync_data(&tmp).unwrap();
    fs.fsync_dir(&meta.join("sorted3/tmp")).unwrap();
    tmp
}

/// The manifest of generation `generation` naming `runs`.
fn manifest(generation: u64, runs: Vec<Run>) -> Vec<u8> {
    Manifest { generation, runs }.encode()
}

/// W4′: a run is built only within what `journal3.ckpt` passed; `MANIFEST` is replaced only
/// once every run it names is durable; a run it replaced is removed only once a durable
/// manifest no longer names it.
#[test]
fn w4_prime_refuses_a_manifest_before_its_runs() {
    let (_dir, root) = root3();
    let meta = root.join("meta");
    let sorted = meta.join("sorted3");
    let path = sorted.join("MANIFEST");
    let fs = OrderCheckFs::new(RealFs);
    fs.atomic_replace(&meta.join("journal3.ckpt"), &ckpt3(0, 2048))
        .unwrap();
    let (first, bytes) = run_bytes(0, 1024);

    // 1. A manifest that names a run not there, or not yet durable: refused.
    assert_breaks_by("W4", "atomic_replace", || {
        fs.atomic_replace(&path, &manifest(1, vec![first])).unwrap();
    });
    let unsynced = sorted.join(first.name());
    fs.append(&unsynced, &bytes).unwrap();
    assert_breaks("W4", || {
        fs.atomic_replace(&path, &manifest(1, vec![first])).unwrap();
    });
    fs.sync_data(&unsynced).unwrap();
    assert_breaks("W4", || {
        fs.atomic_replace(&path, &manifest(1, vec![first])).unwrap();
    });
    fs.fsync_dir(&sorted).unwrap();
    fs.atomic_replace(&path, &manifest(1, vec![first])).unwrap();

    // 2. A run past what the checkpoint passed: refused before its rename.
    let (past, bytes) = run_bytes(1024, 4096);
    let tmp = temp_run(&fs, &meta, &past, &bytes);
    assert_breaks_by("W4", "rename_durable", || {
        fs.rename_durable(&tmp, &sorted.join(past.name())).unwrap();
    });

    // 3. A run within it, merged with the first, published; then the first removed. Removed
    //    while the durable manifest names it: refused.
    let (merged, bytes) = run_bytes(0, 2048);
    let tmp = temp_run(&fs, &meta, &merged, &bytes);
    fs.rename_durable(&tmp, &sorted.join(merged.name()))
        .unwrap();
    assert_breaks_by("W4", "remove", || {
        fs.remove(&sorted.join(first.name())).unwrap();
    });
    fs.atomic_replace(&path, &manifest(2, vec![merged]))
        .unwrap();
    fs.remove(&sorted.join(first.name())).unwrap();
}

/// W9: a frame holding a fact that names a blob is appended only once every object linked in
/// this process is durable, as W5 asks of a changeset's frame.
#[test]
fn w9_a_fact_frame_waits_for_its_objects() {
    let (_dir, root) = root3();
    let meta = root.join("meta");
    let log = meta.join("journal2.log");
    fs::write(meta.join("features"), b"swarmops-features 1\nannotation\n").unwrap();
    let fs = OrderCheckFs::new(RealFs);
    let temp = fs
        .write_durable_temp(&root.join("objects/tmp"), b"value")
        .unwrap();
    fs.link_noclobber(&temp, &root.join("objects/ab/cdef"))
        .unwrap();
    assert_breaks_by("W9", "append_durable", || {
        fs.append_durable(&log, &frame3(&[annotation()], 0))
            .unwrap();
    });
    fs.fsync_dir(&root.join("objects/ab")).unwrap();
    fs.append_durable(&log, &frame3(&[annotation()], 0))
        .unwrap();
}

/// W10: a frame holding a fact of a kind is appended only once `meta/features` lists the kind's
/// feature durably.
#[test]
fn w10_refuses_a_record_before_its_feature() {
    let (_dir, root) = root3();
    let meta = root.join("meta");
    let (log, features) = (meta.join("journal2.log"), meta.join("features"));
    let fs = OrderCheckFs::new(RealFs);

    // 1. No list, then a list without the feature, then one written and not synced.
    assert_breaks_by("W10", "append_durable", || {
        fs.append_durable(&log, &frame3(&[annotation()], 0))
            .unwrap();
    });
    fs.atomic_replace(&features, b"swarmops-features 1\ncheck\n")
        .unwrap();
    assert_breaks("W10", || {
        fs.append_durable(&log, &frame3(&[annotation()], 0))
            .unwrap();
    });
    fs.remove(&features).unwrap();
    fs.append(&features, b"swarmops-features 1\nannotation\n")
        .unwrap();
    assert_breaks("W10", || {
        fs.append_durable(&log, &frame3(&[annotation()], 0))
            .unwrap();
    });

    // 2. Listed durably: appended.
    fs.sync_data(&features).unwrap();
    fs.fsync_dir(&meta).unwrap();
    fs.append_durable(&log, &frame3(&[annotation()], 0))
        .unwrap();
}

/// W11: the poison over `journal2.ckpt` is written only once FORMAT 3's files are durable, and
/// `FORMAT` names FORMAT 3 only once the poison is.
#[test]
fn w11_refuses_format_before_the_poison() {
    let (_dir, root) = root3();
    let meta = root.join("meta");
    fs::write(root.join("FORMAT"), StoreFormat::V2.marker()).unwrap();
    fs::write(meta.join("journal2.ckpt"), [0; 20]).unwrap();
    let (poison, format) = (meta.join("journal2.ckpt"), root.join("FORMAT"));
    let fs = OrderCheckFs::new(RealFs);

    // 1. FORMAT 3 before any poison; the poison before FORMAT 3's files.
    assert_breaks_by("W11", "atomic_replace", || {
        fs.atomic_replace(&format, StoreFormat::V3.marker())
            .unwrap();
    });
    assert_breaks_by("W11", "atomic_replace", || {
        fs.atomic_replace(&poison, UPGRADE_POISON_V3).unwrap();
    });

    // 2. The files written, `facts.col` not synced: the poison waits still.
    let facts = meta.join("idx/facts.col");
    fs.append(&facts, &[0; 24]).unwrap();
    fs.atomic_replace(&meta.join("sorted3/MANIFEST"), &manifest(1, Vec::new()))
        .unwrap();
    fs.atomic_replace(&meta.join("journal3.ckpt"), &ckpt3(0, 0))
        .unwrap();
    assert_breaks("W11", || {
        fs.atomic_replace(&poison, UPGRADE_POISON_V3).unwrap();
    });

    // 3. All durable, its entry too: the poison, then FORMAT.
    fs.sync_data(&facts).unwrap();
    assert_breaks("W11", || {
        fs.atomic_replace(&poison, UPGRADE_POISON_V3).unwrap();
    });
    fs.fsync_dir(&meta.join("idx")).unwrap();
    fs.atomic_replace(&poison, UPGRADE_POISON_V3).unwrap();
    fs.atomic_replace(&format, StoreFormat::V3.marker())
        .unwrap();
}

fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap()
}

fn to_usize(n: u64) -> usize {
    usize::try_from(n).unwrap()
}
