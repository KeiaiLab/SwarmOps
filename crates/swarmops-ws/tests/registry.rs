//! The registry, the no-clobber `REPO` of `init`, and the registry's crash sweep.

use std::sync::{Arc, Barrier};
use std::thread;

use camino::{Utf8Path, Utf8PathBuf};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use swarmops_api::{Identity, OpError, StoreOps, WsListReq};
use swarmops_ops::LocalOps;
use swarmops_store::{FsOps, RealFs};
use swarmops_ws::registry::{ENTRY_FORMAT, Entry, Linked, REGISTRY_DIR, Registry};
use swarmops_ws::state::{self, SWARMWS_DIR, State, Volume, WorkspaceFile};
use swarmops_ws::{Ctx, RealHost};

/// A temporary directory and a store made in it, at `<tmp>/store`.
struct Store {
    _tmp: tempfile::TempDir,
    root: Utf8PathBuf,
    ops: LocalOps,
}

impl Store {
    /// A fresh store named `acme`.
    fn new() -> Store {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap().join("store");
        let (ops, _) = LocalOps::init(&root, "acme").unwrap();
        Store {
            _tmp: tmp,
            root,
            ops,
        }
    }

    /// The store's registry, over the real filesystem.
    fn registry(&self) -> Registry {
        Registry::open(&self.root, Arc::new(RealFs))
    }

    /// What the workspace operations run with, on this store.
    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            store: &self.ops,
            store_root: self.root.clone(),
            who: None,
            fs: Arc::new(RealFs),
            host: Arc::new(RealHost),
        }
    }

    /// A directory beside the store, not made.
    fn beside(&self, name: &str) -> Utf8PathBuf {
        self.root.parent().unwrap().join(name)
    }
}

/// The entry of workspace `name` at `path`, made by `claude-code/<name>`.
fn entry(name: &str, path: &Utf8Path) -> Entry {
    Entry {
        format: ENTRY_FORMAT,
        name: name.to_owned(),
        path: path.to_string(),
        created_by: format!("claude-code/{name}"),
        created_at: 1_792_630_000_000,
        session: None,
    }
}

/// A request or a response from its JSON form.
fn from<T: DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).unwrap()
}

/// The `.json` files of the registry's directory, by name.
fn entry_files(root: &Utf8Path) -> Vec<String> {
    let dir = root.join(REGISTRY_DIR);
    let Ok(read) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = read
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|name| Utf8Path::new(name).extension() == Some("json"))
        .collect();
    names.sort();
    names
}

/// Two threads create one name for two directories at once: one entry is linked, the other
/// create is `E_WS_EXISTS` naming the entry that won, and the entry is the winner's.
#[test]
fn two_creates_of_one_name_have_one_winner() {
    for round in 0..16 {
        // 1. Two creates of `worker` at once, each for its own directory.
        let store = Store::new();
        let barrier = Barrier::new(2);
        let results: Vec<(Entry, Result<Linked, OpError>)> = thread::scope(|scope| {
            let racers: Vec<_> = ["a", "b"]
                .map(|dir| {
                    let (store, barrier) = (&store, &barrier);
                    scope.spawn(move || {
                        let mine = entry("worker", &store.beside(dir));
                        barrier.wait();
                        (mine.clone(), store.registry().create(&mine))
                    })
                })
                .into_iter()
                .collect();
            racers.into_iter().map(|r| r.join().unwrap()).collect()
        });

        // 2. One winner, one E_WS_EXISTS naming it.
        let winners: Vec<&Entry> = (results.iter())
            .filter(|(_, r)| matches!(r, Ok(Linked::New)))
            .map(|(mine, _)| mine)
            .collect();
        assert_eq!(winners.len(), 1, "round {round}: {results:?}");
        let loser = (results.iter())
            .find_map(|(_, r)| r.as_ref().err())
            .expect("one create failed");
        assert_eq!(loser.code, "E_WS_EXISTS", "round {round}: {loser:?}");
        assert_eq!(loser.details["name"], "worker");
        assert_eq!(loser.details["path"], winners[0].path.as_str());

        // 3. The entry is the winner's, the only one, and no temp is left beside it.
        let read = store.registry().read("worker").unwrap();
        assert_eq!(read.as_ref(), Some(winners[0]), "round {round}");
        assert_eq!(entry_files(&store.root), ["worker.json"]);
        let tmp = store.root.join(REGISTRY_DIR).join("tmp");
        assert_eq!(std::fs::read_dir(tmp).unwrap().count(), 0, "round {round}");
    }
}

/// A create again with the same name, path, creator and session is the same workspace: the
/// entry made first, untouched, whatever its time.
#[test]
fn a_create_with_the_same_arguments_is_the_same_workspace() {
    // 1. A first create links the entry.
    let store = Store::new();
    let registry = store.registry();
    let first = entry("worker", &store.beside("w"));
    assert_eq!(registry.create(&first).unwrap(), Linked::New);

    // 2. The same arguments later: the existing entry, nothing written.
    let again = Entry {
        created_at: first.created_at + 5_000,
        ..first.clone()
    };
    assert_eq!(
        registry.create(&again).unwrap(),
        Linked::Existing(first.clone())
    );
    assert_eq!(registry.read("worker").unwrap(), Some(first.clone()));

    // 3. So with a session, the same one on both.
    let hooked = Entry {
        name: "hooked".to_owned(),
        session: Some("0123456789abcdef".to_owned()),
        ..first.clone()
    };
    assert_eq!(registry.create(&hooked).unwrap(), Linked::New);
    assert_eq!(
        registry.create(&hooked).unwrap(),
        Linked::Existing(hooked.clone())
    );

    // 4. Another path or another creator is another workspace: E_WS_EXISTS.
    let elsewhere = Entry {
        path: store.beside("other").to_string(),
        ..first.clone()
    };
    assert_eq!(registry.create(&elsewhere).unwrap_err().code, "E_WS_EXISTS");
    let someone = Entry {
        created_by: "claude-code/other".to_owned(),
        ..first.clone()
    };
    assert_eq!(registry.create(&someone).unwrap_err().code, "E_WS_EXISTS");
    assert_eq!(registry.read("worker").unwrap(), Some(first));
}

/// The create rule's session arm: an entry with another session, or with none where the
/// existing one has one (and the reverse), is another workspace.
#[test]
fn a_create_with_another_session_is_ws_exists() {
    // 1. A hook-made entry, with its session.
    let store = Store::new();
    let registry = store.registry();
    let hooked = Entry {
        session: Some("0123456789abcdef".to_owned()),
        ..entry("worker", &store.beside("w"))
    };
    assert_eq!(registry.create(&hooked).unwrap(), Linked::New);

    // 2. Another session, and none, are refused, naming the entry there.
    for session in [Some("fedcba9876543210".to_owned()), None] {
        let other = Entry {
            session,
            ..hooked.clone()
        };
        let refused = registry.create(&other).unwrap_err();
        assert_eq!(refused.code, "E_WS_EXISTS", "{refused:?}");
        assert_eq!(refused.details["path"], hooked.path.as_str());
    }

    // 3. A hand-made entry is refused to a hook's create of the same name.
    let hand = entry("manual", &store.beside("m"));
    assert_eq!(registry.create(&hand).unwrap(), Linked::New);
    let by_hook = Entry {
        session: Some("0123456789abcdef".to_owned()),
        ..hand.clone()
    };
    assert_eq!(registry.create(&by_hook).unwrap_err().code, "E_WS_EXISTS");
    assert_eq!(registry.read("manual").unwrap(), Some(hand));
}

/// A name Windows takes for a device, or one that ends in a dot, is refused on every platform,
/// by create, read and delete alike, and nothing is written.
#[test]
fn a_workspace_named_after_a_windows_device_is_refused_everywhere() {
    let store = Store::new();
    let registry = store.registry();
    for name in [
        "con",
        "nul",
        "aux.txt",
        "com1",
        "lpt9.tar.gz",
        "prn",
        "worker.",
    ] {
        // 1. Each name, refused before anything is written.
        let refused = registry
            .create(&entry(name, &store.beside("w")))
            .unwrap_err();
        assert_eq!(refused.code, "E_INVALID_ARG", "{name}: {refused:?}");
        assert!(refused.message.contains(name), "{name}: {refused:?}");
        assert_eq!(registry.read(name).unwrap_err().code, "E_INVALID_ARG");
        assert_eq!(registry.delete(name).unwrap_err().code, "E_INVALID_ARG");
    }
    assert_eq!(entry_files(&store.root), Vec::<String>::new());

    // 2. A name that merely holds a device's letters is a name.
    let fine = entry("console", &store.beside("w"));
    assert_eq!(registry.create(&fine).unwrap(), Linked::New);
}

/// A store with registry entries is a store as before: it opens, verifies deep clean, and every
/// read operation answers as it did, none of them listing or reading the registry.
#[test]
fn an_older_store_reader_ignores_the_registry() {
    // 1. A store with one commit on main.
    let store = Store::new();
    let who = Identity::parse("tester/one").unwrap();
    let commit = (store.ops)
        .commit(
            &who,
            from(json!({
                "changes": [{"path": "src/a.txt", "op": "put", "content": {"inline": "a\n"}}],
                "message": "first",
                "advance": "main",
            })),
        )
        .unwrap();

    // 2. Every read before the registry has entries.
    let reads = |ops: &LocalOps| -> Vec<Value> {
        let id = commit.changeset.as_str();
        vec![
            serde_json::to_value(ops.repo_info(from(json!({}))).unwrap()).unwrap(),
            serde_json::to_value(ops.ref_list(from(json!({}))).unwrap()).unwrap(),
            serde_json::to_value(ops.log(from(json!({"rev": "main"}))).unwrap()).unwrap(),
            serde_json::to_value(ops.show(from(json!({"rev": id}))).unwrap()).unwrap(),
            serde_json::to_value(ops.read_tree(from(json!({"rev": "main"}))).unwrap()).unwrap(),
            serde_json::to_value(
                ops.read_file(from(json!({"rev": "main", "path": "src/a.txt"})))
                    .unwrap(),
            )
            .unwrap(),
            serde_json::to_value(
                ops.path_history(from(json!({"rev": "main", "path": "src/a.txt"})))
                    .unwrap(),
            )
            .unwrap(),
            serde_json::to_value(ops.op_log(from(json!({}))).unwrap()).unwrap(),
        ]
    };
    let before = reads(&store.ops);

    // 3. Two entries, and a leftover temp beside them.
    let registry = store.registry();
    registry
        .create(&entry("one", &store.beside("one")))
        .unwrap();
    registry
        .create(&entry("two", &store.beside("two")))
        .unwrap();
    let tmp = store.root.join(REGISTRY_DIR).join("tmp");
    std::fs::write(tmp.join("leftover"), b"{").unwrap();

    // 4. Opened anew: deep verify clean, every read as before.
    let reopened = LocalOps::open(&store.root).unwrap();
    let report = reopened.verify(from(json!({"deep": true}))).unwrap();
    assert_eq!(report.problems, Vec::<String>::new());
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert_eq!(reads(&reopened), before);
}

/// Two `init`s of one directory at once: one makes the store, the other is `E_EXISTS`, and the
/// store's `REPO` is the winner's, unchanged by the loser.
#[test]
fn concurrent_init_on_one_directory_has_one_winner() {
    for round in 0..16 {
        // 1. Two inits of one path at once.
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap().join("store");
        let barrier = Barrier::new(2);
        let results: Vec<Result<String, OpError>> = thread::scope(|scope| {
            let racers: Vec<_> = ["first", "second"]
                .map(|name| {
                    let (root, barrier) = (&root, &barrier);
                    scope.spawn(move || {
                        barrier.wait();
                        LocalOps::init(root, name).map(|(_, info)| info.repo_id)
                    })
                })
                .into_iter()
                .collect();
            racers.into_iter().map(|r| r.join().unwrap()).collect()
        });

        // 2. One winner; the loser is E_EXISTS.
        let won: Vec<&String> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
        assert_eq!(won.len(), 1, "round {round}: {results:?}");
        let lost = results.iter().find_map(|r| r.as_ref().err()).unwrap();
        assert_eq!(lost.code, "E_EXISTS", "round {round}: {lost:?}");

        // 3. REPO names the winner's id, and the store opens as the winner's.
        let repo = std::fs::read_to_string(root.join("REPO")).unwrap();
        assert!(repo.starts_with(&format!("{}\n", won[0])), "round {round}");
        let info = LocalOps::open(&root)
            .unwrap()
            .repo_info(from(json!({})))
            .unwrap();
        assert_eq!(&info.repo_id, won[0], "round {round}");
    }
}

/// `ws list` reads the registry; with `check` it opens each workspace too, and one whose
/// directory is gone is an orphan, its entry kept.
#[test]
fn list_check_reports_a_missing_directory_as_orphan() {
    // 1. Two workspaces with their directories, then one directory removed.
    let store = Store::new();
    let registry = store.registry();
    for name in ["kept", "gone"] {
        let dir = store.beside(name);
        std::fs::create_dir_all(dir.join(SWARMWS_DIR)).unwrap();
        state::replace(&RealFs, &dir, &workspace(name)).unwrap();
        registry.create(&entry(name, &dir)).unwrap();
    }
    std::fs::remove_dir_all(store.beside("gone")).unwrap();
    let ctx = store.ctx();

    // 2. Without check: both, by name, neither an orphan, no directory read.
    let listed = swarmops_ws::list(&ctx, WsListReq { check: false }).unwrap();
    let names: Vec<&str> = listed.workspaces.iter().map(|w| w.name.as_str()).collect();
    assert_eq!(names, ["gone", "kept"]);
    assert!(listed.workspaces.iter().all(|w| !w.orphan));

    // 3. With check: `gone` is an orphan; `kept` carries its state.
    let checked = swarmops_ws::list(&ctx, WsListReq { check: true }).unwrap();
    let [gone, kept] = checked.workspaces.as_slice() else {
        panic!("{checked:?}");
    };
    assert!(gone.orphan, "{gone:?}");
    assert_eq!(gone.path, store.beside("gone").as_str());
    assert!(!kept.orphan, "{kept:?}");
    assert_eq!(kept.head, "c".repeat(64));
    assert_eq!(kept.prefixes, ["src"]);
    assert_eq!(entry_files(&store.root), ["gone.json", "kept.json"]);

    // 4. `ws info` of the kept one reads the same, with its status; of an unknown one, not
    //    found.
    let info = swarmops_ws::info(&ctx, from(json!({"workspace": "kept"}))).unwrap();
    let without_status = swarmops_api::WsInfo {
        status_after: None,
        ..info
    };
    assert_eq!(&without_status, kept);
    let unknown = swarmops_ws::info(&ctx, from(json!({"workspace": "nobody"}))).unwrap_err();
    assert_eq!(unknown.code, "E_WS_NOT_FOUND");
}

/// A workspace's state, as `ws create` records one for `name`.
fn workspace(name: &str) -> WorkspaceFile {
    WorkspaceFile {
        format: state::WORKSPACE_FORMAT,
        name: name.to_owned(),
        repo: "/stores/acme".to_owned(),
        repo_id: "0".repeat(32),
        base: "b".repeat(64),
        head: "c".repeat(64),
        prefixes: vec!["src".to_owned()],
        all: false,
        created_by: format!("claude-code/{name}"),
        created_at: 1_792_630_000_000,
        target: "main".to_owned(),
        state: State::Clean,
        onto: None,
        volume: Volume {
            case_insensitive: false,
            exec_bit: true,
            symlinks: true,
        },
        materialized: "c".repeat(64),
    }
}

/// A create and a delete of an entry, cut at each of their filesystem calls in turn (every
/// mutating call from the k-th on fails, as a crash there leaves the files): a create run again
/// links the entry or finds it, a delete run again removes it, and the registry holds the entry
/// or nothing, never a part of it.
#[test]
#[cfg(feature = "test-hooks")]
fn a_crash_during_create_or_delete_leaves_the_entry_or_nothing() {
    use swarmops_store::{CrashAtFs, FailAt};

    /// The registry of `root` over `fs`.
    fn over(root: &Utf8Path, fs: Arc<dyn FsOps>) -> Registry {
        Registry::open(root, fs)
    }

    /// The registry of `root` failing every mutating call from the k-th on.
    fn cut_at(root: &Utf8Path, k: u64) -> Registry {
        let fail = FailAt::parse(&format!("{k}:persistent")).unwrap().unwrap();
        over(root, Arc::new(swarmops_store::FailAtFs::new(RealFs, fail)))
    }

    // 1. How many calls a create and a delete make, counted on a store of their own.
    let probe = Store::new();
    let counter = Arc::new(CrashAtFs::new(RealFs, 0));
    let mine = entry("worker", &probe.beside("w"));
    over(&probe.root, counter.clone()).create(&mine).unwrap();
    let creates = counter.calls();
    over(&probe.root, counter.clone()).delete("worker").unwrap();
    let deletes = counter.calls() - creates;
    assert!(creates >= 4 && deletes >= 2, "{creates} {deletes}");

    // 2. A create cut at each call: what is there is the entry or nothing; run again, it is
    //    the entry.
    for k in 1..=creates + 1 {
        let store = Store::new();
        let mine = entry("worker", &store.beside("w"));
        let cut = cut_at(&store.root, k).create(&mine);
        let found = store.registry().read("worker").unwrap();
        assert!(
            found.is_none() || found.as_ref() == Some(&mine),
            "k={k}: {cut:?}"
        );
        let again = store.registry().create(&mine).unwrap();
        assert!(matches!(again, Linked::New | Linked::Existing(_)), "k={k}");
        assert_eq!(
            store.registry().read("worker").unwrap(),
            Some(mine),
            "k={k}"
        );
        assert_eq!(entry_files(&store.root), ["worker.json"], "k={k}");
    }

    // 3. A delete cut at each call: run again, the entry is gone.
    for k in 1..=deletes + 1 {
        let store = Store::new();
        let mine = entry("worker", &store.beside("w"));
        store.registry().create(&mine).unwrap();
        let cut = cut_at(&store.root, k).delete("worker");
        let found = store.registry().read("worker").unwrap();
        assert!(
            found.is_none() || found.as_ref() == Some(&mine),
            "k={k}: {cut:?}"
        );
        store.registry().delete("worker").unwrap();
        assert_eq!(store.registry().read("worker").unwrap(), None, "k={k}");
        assert_eq!(entry_files(&store.root), Vec::<String>::new(), "k={k}");
    }
}
