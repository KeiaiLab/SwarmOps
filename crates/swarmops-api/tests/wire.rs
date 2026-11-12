//! The native wire's messages (native sync design §2.2): each request and result is pinned by a
//! golden file in `tests/golden_wire/`, one JSON line as the wire carries it.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::fs;
use std::path::PathBuf;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use swarmops_api::wire::{
    MAX_CHUNK_BYTES, MAX_WIRE_IDS, MAX_WIRE_LINE, WIRE_FACTS, WIRE_GET, WIRE_HAS, WIRE_HELLO,
    WIRE_LAND, WIRE_LIST, WIRE_MOVE, WIRE_PROTOCOL, WIRE_PUT, WIRE_REFS, WIRE_STALL_SECS,
    WireBlobWant, WireChunks, WireCursor, WireGetReq, WireHas, WireHasReq, WireHello, WireHelloReq,
    WireLandReq, WireList, WireListReq, WireMove, WireMoveMode, WireMoveReq, WirePut, WirePutReq,
    WireRef, WireRefs, WireRefsReq, WireRequest,
};
use swarmops_api::{
    ConflictKindOut, ConflictReport, Envelope, ErrorCode, FileConflictOut, LandResp, LandStatus,
    Meta, OpError,
};

/// The directory of the golden files.
fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden_wire")
}

/// A changeset or blob id: 64 lowercase hex digits, `digit` repeated.
fn id(digit: char) -> String {
    digit.to_string().repeat(64)
}

/// What a text decodes to, against a pinned value.
#[derive(Debug, PartialEq, Eq)]
enum Decoded {
    /// The text is refused.
    Refused,
    /// It decodes to the pinned value.
    Same,
    /// It decodes to another value.
    Differs,
}

/// Decodes a text against a pinned value.
type Decode = Box<dyn Fn(&str) -> Decoded>;

/// One pinned message: its file's name, its encoding, and how a text decodes against it.
struct Golden {
    name: &'static str,
    encoded: String,
    same: Decode,
}

fn golden<T>(name: &'static str, value: T) -> Golden
where
    T: Serialize + DeserializeOwned + PartialEq + Debug + 'static,
{
    let encoded = serde_json::to_string(&value).unwrap();
    let same = move |text: &str| match serde_json::from_str::<T>(text) {
        Err(_) => Decoded::Refused,
        Ok(got) if got == value => Decoded::Same,
        Ok(_) => Decoded::Differs,
    };
    Golden {
        name,
        encoded,
        same: Box::new(same),
    }
}

/// A request line of `op`, under this protocol.
fn request<T>(name: &'static str, op: &str, req: T) -> Golden
where
    T: Serialize + DeserializeOwned + PartialEq + Debug + 'static,
{
    golden(name, WireRequest::new(op, WIRE_PROTOCOL, req))
}

/// Every message, each field set to a value other than its default, so that a field lost on
/// the way out or in shows.
fn goldens() -> Vec<Golden> {
    // 1. Requests, as the lines a client writes.
    let mut hello = WireHelloReq::new(1, 2, vec!["blob".into(), "changeset".into()], vec![]);
    hello.features = vec!["lineage".into()];
    let mut refs = WireRefsReq::new();
    refs.after = Some("release/1".into());
    refs.limit = 2;
    let mut list = WireListReq::new(id('a'));
    list.have = vec![id('b')];
    list.after = Some(WireCursor::new(7, id('c')));
    list.limit = 3;
    let mut get = WireGetReq::new("main");
    get.changesets = vec![id('a')];
    get.blobs = vec![WireBlobWant::new(id('a'), id('d'))];
    let mut put = WirePutReq::new(2, 4_096);
    put.adopt = vec![id('b'), id('a')];
    let mut land = WireLandReq::new("main", id('b'), id('a'));
    land.max_attempts = 3;
    land.max_conflict_bytes = 1_024;
    let mut move_ = WireMoveReq::new("feature/x");
    move_.old = Some(id('b'));
    move_.new = Some(id('a'));
    move_.mode = WireMoveMode::Force;

    // 2. Results, as the `result` of an answer's envelope.
    let mut refs_page = WireRefs::new(vec![WireRef::new("main", id('a'))], true);
    refs_page.next = Some("main".into());
    let mut list_page = WireList::new(vec![id('a'), id('b')]);
    list_page.next = Some(WireCursor::new(5, id('b')));

    vec![
        request("hello_req", WIRE_HELLO, hello),
        request("refs_req", WIRE_REFS, refs),
        request("list_req", WIRE_LIST, list),
        request("get_req", WIRE_GET, get),
        request("has_req", WIRE_HAS, WireHasReq::new(vec![id('a'), id('d')])),
        request("put_req", WIRE_PUT, put),
        request("land_req", WIRE_LAND, land),
        request("move_req", WIRE_MOVE, move_),
        golden(
            "hello",
            WireHello::new(
                1,
                "person/ada",
                3,
                vec!["blob".into(), "changeset".into()],
                vec!["lineage".into()],
                "0.12.0",
            ),
        ),
        golden("refs", refs_page),
        golden("list", list_page),
        golden("get", WireChunks::new(2, 4_096)),
        golden("has", WireHas::new(vec![true, false])),
        golden("put", WirePut::new(vec![id('b')], vec![id('a')])),
        golden(
            "move",
            WireMove::new("feature/x", Some(id('b')), Some(id('a'))),
        ),
    ]
}

/// Each message encodes to its golden file's line, and that line decodes to the message; no
/// golden file is left without a message.
#[test]
fn every_wire_message_matches_its_golden() {
    let goldens = goldens();
    for golden in &goldens {
        let path = golden_dir().join(format!("{}.json", golden.name));
        let file = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", golden.name));
        assert_eq!(file, format!("{}\n", golden.encoded), "{}", golden.name);
        assert_eq!((golden.same)(&file), Decoded::Same, "{}", golden.name);
    }

    // Every file is a pinned message's.
    let mut files: Vec<String> = fs::read_dir(golden_dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    files.sort();
    let mut named: Vec<String> = goldens.iter().map(|g| format!("{}.json", g.name)).collect();
    named.sort();
    assert_eq!(files, named);
}

/// Every key path of `value`, outermost first: `["req", "after", "generation"]`.
fn key_paths(value: &Value, prefix: &[String], out: &mut Vec<Vec<String>>) {
    let Value::Object(map) = value else {
        return;
    };
    for (key, inner) in map {
        let mut path = prefix.to_vec();
        path.push(key.clone());
        out.push(path.clone());
        key_paths(inner, &path, out);
    }
}

/// `value` with the key at `path` renamed, as a writer that misspelt one field would send it.
fn renamed(value: &Value, path: &[String]) -> Value {
    let mut copy = value.clone();
    let mut map: &mut Map<String, Value> = copy.as_object_mut().unwrap();
    let (last, outer) = path.split_last().unwrap();
    for key in outer {
        map = map.get_mut(key).unwrap().as_object_mut().unwrap();
    }
    let inner = map.remove(last).unwrap();
    map.insert(format!("{last}_renamed"), inner);
    copy
}

/// The golden test's own check: renaming any one field of any golden line, at any depth, makes
/// the line refused or decode to another value, so a golden can pin no field by accident.
#[test]
fn a_flipped_field_in_any_golden_is_refused_or_differs() {
    for golden in goldens() {
        let value: Value = serde_json::from_str(&golden.encoded).unwrap();
        let mut paths = Vec::new();
        key_paths(&value, &[], &mut paths);
        assert!(!paths.is_empty(), "{}", golden.name);
        for path in paths {
            let text = renamed(&value, &path).to_string();
            assert_ne!(
                (golden.same)(&text),
                Decoded::Same,
                "{}: renaming {path:?} went unnoticed",
                golden.name
            );
        }
    }
}

/// The wire's bounds and names are the ones the design documents.
#[test]
fn wire_constants_are_the_documented_values() {
    assert_eq!(WIRE_PROTOCOL, 1);
    assert_eq!(MAX_WIRE_LINE, 1024 * 1024);
    assert_eq!(MAX_WIRE_IDS, 10_000);
    assert_eq!(MAX_CHUNK_BYTES, 64 * 1024 * 1024);
    assert_eq!(WIRE_STALL_SECS, 300);
    let ops = [
        WIRE_HELLO, WIRE_REFS, WIRE_LIST, WIRE_GET, WIRE_HAS, WIRE_PUT, WIRE_LAND, WIRE_MOVE,
        WIRE_FACTS,
    ];
    let names = [
        "wire_hello",
        "wire_refs",
        "wire_list",
        "wire_get",
        "wire_has",
        "wire_put",
        "wire_land",
        "wire_move",
        "wire_facts",
    ];
    assert_eq!(ops, names);
}

/// An answer is one line holding the existing envelope: a land's `LandResp`, or a refusal with
/// its `ConflictReport` in `details`, each within `MAX_WIRE_LINE` and read back unchanged.
#[test]
fn an_envelope_line_of_a_wire_answer_round_trips() {
    // 1. A landed change.
    let landed = LandResp::new(
        LandStatus::Landed,
        id('e'),
        id('b'),
        "main",
        id('b'),
        1,
        false,
        2,
        vec![id('a')],
    );
    let ok = Envelope::ok(WIRE_LAND, landed, Meta::new(3, BTreeMap::new(), "ssh"));
    round_trips(&ok);

    // 2. A refused one, with the paths both sides touched.
    let file = FileConflictOut::new("src/lib.rs", ConflictKindOut::Content);
    let kinds = vec![ConflictKindOut::Content];
    let report = ConflictReport::new(id('b'), id('a'), id('f'), vec![file], kinds, false);
    let mut refusal = OpError::new(ErrorCode::Conflict, "the change conflicts on main");
    let Value::Object(details) = serde_json::to_value(&report).unwrap() else {
        unreachable!("a report is an object");
    };
    refusal.details = details;
    let err: Envelope<LandResp> =
        Envelope::err(WIRE_LAND, refusal, Meta::new(3, BTreeMap::new(), "ssh"));
    round_trips(&err);
}

#[track_caller]
fn round_trips(envelope: &Envelope<LandResp>) {
    let line = serde_json::to_string(envelope).unwrap();
    assert!(!line.contains('\n'));
    assert!(line.len() <= MAX_WIRE_LINE);
    let back: Envelope<LandResp> = serde_json::from_str(&line).unwrap();
    assert_eq!(&back, envelope);
}

/// Rewrites the golden files from `goldens()`. Writes nothing unless `SWARMOPS_REGEN_GOLDEN=1`
/// is set, so `--run-ignored all` never rewrites the pin. Run on purpose, review the diff,
/// commit; `every_wire_message_matches_its_golden` then pins it.
#[test]
#[ignore = "writes tests/golden_wire when SWARMOPS_REGEN_GOLDEN=1; run on purpose"]
fn regenerate_wire_golden() {
    if std::env::var_os("SWARMOPS_REGEN_GOLDEN").is_none() {
        eprintln!("regenerate_wire_golden: SWARMOPS_REGEN_GOLDEN not set; nothing written");
        return;
    }
    fs::create_dir_all(golden_dir()).unwrap();
    for golden in goldens() {
        let path = golden_dir().join(format!("{}.json", golden.name));
        fs::write(path, format!("{}\n", golden.encoded)).unwrap();
    }
}
