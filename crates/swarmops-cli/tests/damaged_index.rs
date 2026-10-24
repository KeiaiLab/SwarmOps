//! A damaged or crafted changeset index through the binary (SECURITY.md, "Crafted or damaged
//! store files"): a sorted-view entry that names another changeset's record, or a `par.col`
//! entry moved onto another record's parents, fails each command that reads it with
//! `E_STORE_CORRUPT`, and writes nothing, rather than answering with another changeset or another
//! history; a revision's `~N` walks the changeset objects, so that it names what it names
//! whatever the index says of the records it passes. `verify --deep` reports the damage.

use std::fs;
use std::sync::Arc;
use std::time::Duration;

use camino::Utf8PathBuf;
use swarmops_core::ObjectId;
use swarmops_store::{Index, Layout, MetaLock, MetaStore, RealFs};

use crate::support::Store;

/// The agent the tests commit as.
const AGENT: &str = "agent/worker-17";

/// A store holding a first-parent chain of five changesets, `c1` (a root) to `c5`, each putting
/// `f` holding its number, with `main` at `c2`; checkpointed and compacted through the library,
/// so that the index answers every lookup on its own: the sorted views cover every record, and
/// no frame lies past the checkpoint. Their ids, `c1` first.
fn chain() -> (Store, [String; 5]) {
    let store = Store::init();
    let mut ids: Vec<String> = Vec::new();
    for n in 1..=5 {
        let parent = ids.last().map(String::as_str);
        ids.push(store.commit(AGENT, "f", &n.to_string(), parent));
    }
    let main = ["ref", "set", "main", &ids[1], "--create"];
    store.run_as(AGENT, &main, &[]).ok();
    let root = Utf8PathBuf::from(store.path());
    MetaStore::open(&root).unwrap().flush().unwrap();
    let layout = Layout::new(&root);
    let _lock = MetaLock::acquire(&layout, Duration::from_secs(10)).unwrap();
    let mut index = Index::open(&layout, Arc::new(RealFs::new())).unwrap();
    assert_eq!(index.compact().unwrap(), 5);
    (store, ids.try_into().unwrap())
}

/// The content of `f` at revision `rev`, through `cat`.
fn f_at(store: &Store, rev: &str) -> String {
    let cat = store.read(&["cat", &format!("{rev}:f")]).ok();
    cat["content"].as_str().unwrap().to_owned()
}

#[test]
fn a_view_entry_naming_another_changeset_fails_show_cat_log_and_ref_set() {
    let (store, [c1, c2, c3, ..]) = chain();
    // c1's entry in the id view names c3's record (2) in sorted/ids.rec.
    let layout = store.layout();
    let keys = fs::read(layout.meta_path("sorted/ids.key")).unwrap();
    let key = ObjectId::from_hex(&c1).unwrap().0;
    let at = (keys.as_chunks::<32>().0.iter())
        .position(|entry| *entry == key)
        .unwrap();
    let recs_file = layout.meta_path("sorted/ids.rec");
    let mut recs = fs::read(&recs_file).unwrap();
    recs[at * 8..(at + 1) * 8].copy_from_slice(&2_u64.to_le_bytes());
    fs::write(&recs_file, recs).unwrap();

    let prefix = &c1[..12];
    let cat = format!("{prefix}:f");
    for args in [
        &["show", &c1][..],
        &["show", prefix],
        &["cat", &cat],
        &["log", prefix],
    ] {
        store.read(args).err("E_STORE_CORRUPT");
    }
    let journal = store.journal_len();
    let set = ["ref", "set", "main", prefix, "--expect", &c2];
    store.run_as(AGENT, &set, &[]).err("E_STORE_CORRUPT");
    assert_eq!(store.journal_len(), journal, "no ref move is journaled");
    // c3 itself reads as it is.
    assert_eq!(store.show(&c3)["id"], c3.as_str());
    assert_eq!(f_at(&store, &c3), "3");
    assert_eq!(
        store.problems(),
        [0, 2].map(|rec| format!(
            "changeset record {rec}: a sorted view disagrees with the index columns; rebuild \
             the sorted views from the columns"
        ))
    );
}

#[test]
fn a_par_col_entry_out_of_place_fails_log_and_tilde_walks_the_objects() {
    let (store, [_, _, c3, c4, c5]) = chain();
    // c3's par.col entry (record 2) points at c2's parents, c1, instead of at its own, c2: one
    // bit, 0x2001 to 0x0001.
    let par = store.layout().meta_path("idx/par.col");
    let mut entries = fs::read(&par).unwrap();
    assert_eq!(entries[16..24], ((32_u64 << 8) | 1).to_le_bytes());
    entries[16..24].copy_from_slice(&1_u64.to_le_bytes());
    fs::write(&par, entries).unwrap();

    // A read of c3's record, or of c4's, whose parents no longer start where c3's end, fails.
    let c3_1 = format!("{c3}~1:f");
    let c4_2 = format!("{c4}~2:f");
    for args in [
        &["log", &c3][..],
        &["log", &c5],
        &["show", &c3],
        &["cat", &c3_1],
        &["cat", &c4_2],
    ] {
        store.read(args).err("E_STORE_CORRUPT");
    }
    // From c5 the walk reads the objects: three first parents back is c2, not c1.
    assert_eq!(f_at(&store, &format!("{c5}~3")), "2");
    let mismatch = "changeset record 2: the index disagrees with the journal; rebuild the index \
                    from the journal";
    assert_eq!(store.problems(), [mismatch]);
}

#[test]
fn a_truncated_verify_exits_like_an_untruncated_one_with_problems() {
    let (store, [c1, ..]) = chain();
    // c1's id-view entry naming c3's record, and c3's par.col entry out of place: two problems.
    let layout = store.layout();
    let keys = fs::read(layout.meta_path("sorted/ids.key")).unwrap();
    let key = ObjectId::from_hex(&c1).unwrap().0;
    let at = (keys.as_chunks::<32>().0.iter())
        .position(|entry| *entry == key)
        .unwrap();
    let recs_file = layout.meta_path("sorted/ids.rec");
    let mut recs = fs::read(&recs_file).unwrap();
    recs[at * 8..(at + 1) * 8].copy_from_slice(&2_u64.to_le_bytes());
    fs::write(&recs_file, recs).unwrap();
    let par = layout.meta_path("idx/par.col");
    let mut entries = fs::read(&par).unwrap();
    entries[16..24].copy_from_slice(&1_u64.to_le_bytes());
    fs::write(&par, entries).unwrap();

    let full = store.read(&["verify", "--deep"]);
    let full_exit = full.exit();
    let full = full.err("E_STORE_CORRUPT")["details"].take();
    let total = full["problems"].as_array().unwrap().len();
    assert!(total >= 2, "{full}");
    assert_eq!(full["truncated"], false);

    // One finding listed, the rest only counted; the exit code is the same.
    let cut = store.read(&["verify", "--deep", "--max-findings", "1"]);
    assert_eq!(cut.exit(), full_exit);
    let cut = cut.err("E_STORE_CORRUPT")["details"].take();
    assert_eq!(cut["problems"].as_array().unwrap().len(), 1, "{cut}");
    assert_eq!(cut["findings"].as_array().unwrap().len(), 1, "{cut}");
    assert_eq!(cut["found"], full["found"]);
    assert_eq!(cut["problems"][0], full["problems"][0]);
    assert_eq!(cut["truncated"], true);
}

#[test]
fn verify_refuses_a_max_findings_of_0_or_over_the_cap() {
    let (store, _) = chain();
    for bad in ["0", "1001"] {
        let run = store.read(&["verify", "--max-findings", bad]);
        run.err("E_INVALID_ARG");
        assert_eq!(run.exit(), 2, "{bad}");
    }
    let top = store.read(&["verify", "--max-findings", "1000"]).ok();
    assert_eq!(top["truncated"], false);
}
