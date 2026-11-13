//! The upstream's side of native sync (the native sync design §2): `swarmops serve --stdio
//! '<store>'`, asked for through the key's forced command, answers the wire of
//! [`swarmops_api::wire`] on stdin and stdout until the client closes stdin.
//!
//! ```text
//! 1 store under --root ─▶ 2 FORMAT 2 or later ─▶ 3 access.toml: read      (else one refusal line)
//! 4 loop: one request line ─▶ api, protocol (hello: its range) ─▶ handler
//!         ─▶ one Envelope line [+ the chunks a wire_get announces]        … EOF ends, exit 0
//!
//!   hello  refs  list  get      read, checked at 3
//!   has    put                  push or land on some ref, access.toml read again
//!   land                        land on the target, read again before each attempt
//!   move                        push, force or delete on the ref, read again
//! ```
//!
//! A refusal is an envelope line like any answer, its texts masked by the [`Redactor`] of
//! `--root`. After a refusal that leaves the input unreadable (a `wire_put` whose chunks were not
//! all read, a line that is no JSON object or is past [`MAX_WIRE_LINE`]) the session ends with
//! the refusal's exit code. Only objects some listed ref reaches are sent (V9): a changeset under
//! the ref asked for, a blob the delta of such a changeset names; anything else is answered as an
//! object the store lacks. A land is keyed by `sync_key` of its target and head, so one change
//! lands once whoever sends it; a move goes through `ref set` and `ref delete` (V10).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, BufRead, Read as _, Write};
use std::time::Instant;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use swarmops_api::wire::{
    WIRE_GET, WIRE_HAS, WIRE_HELLO, WIRE_LAND, WIRE_LIST, WIRE_MOVE, WIRE_PUT, WIRE_REFS,
};
use swarmops_api::{
    API_VERSION, Envelope, ErrorCode, Identity, LandReq, MAX_CHUNK_BYTES, MAX_WIRE_IDS,
    MAX_WIRE_LINE, Meta, OpError, RefDeleteReq, RefSetReq, Rev, StoreOps as _, WIRE_PROTOCOL,
    WireChunks, WireCursor, WireGetReq, WireHas, WireHasReq, WireHello, WireHelloReq, WireLandReq,
    WireList, WireListReq, WireMove, WireMoveMode, WireMoveReq, WirePut, WirePutReq, WireRef,
    WireRefs, WireRefsReq,
};
use swarmops_core::{BlobId, ChangesetId, DeltaOp, Hashed, ObjectId, ObjectKind, bytes};
use swarmops_repo::{RepoError, RepoRead};
use swarmops_store::{FsObjectStore, RefKind, StoreError, StoreFormat, ref_kind};

use super::access::{Access, Action};
use super::entry::store_under;
use super::guard::Redactor;
use super::receive::MAX_PUSH_BYTES;
use super::{ServeInvocation, Served, refused, reply_then_step, step_store};
use crate::identity::AUTH_TRUST;
use crate::native::chunk::{self, HEADER_LEN, Takes};
use crate::native::key::sync_key;
use crate::{LocalOps, error};

/// The op an answer names when no request named one: a refusal before the first request.
const SESSION_OP: &str = "serve";

/// The object kinds this server sends and takes, by name.
const OBJECT_KINDS: [&str; 2] = ["blob", "changeset"];

/// A record's header in a chunk: kind, id, payload length.
const RECORD_HEADER_LEN: u64 = 37;

/// The most bytes of names and ids one `wire_refs` page carries, so that its answer stays
/// within [`MAX_WIRE_LINE`].
const REFS_PAGE_BYTES: usize = MAX_WIRE_LINE / 2;

/// The bytes of JSON one listed ref adds to a page besides its name and id.
const REF_JSON_BYTES: usize = 32;

/// The most bytes of hunk text a native land's conflict report carries, so that the answer
/// stays within [`MAX_WIRE_LINE`].
const MAX_NATIVE_CONFLICT_BYTES: u64 = (MAX_WIRE_LINE / 2) as u64;

/// The end of a line.
const NEWLINE: u8 = b'\n';

/// A snapshot of the served store.
type Snapshot<'a> = RepoRead<'a, FsObjectStore>;

/// Serves native sync's session on the store named `store` to `who`, on this process's stdin
/// and stdout. The view work its writes left is stepped once the session has ended.
pub fn run(invocation: &ServeInvocation, who: &Identity, store: &str) -> Result<Served, OpError> {
    let redactor = Redactor::new(&[invocation.root.as_std_path()]);
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();

    // 1. The store, opened and checked; a refusal is the session's one line.
    let opened = open(invocation, who, store);
    let (path, ops) = match opened {
        Ok(opened) => opened,
        Err(e) => return Ok(refuse_session(&mut output, e, &redactor)),
    };

    // 2. The requests, then one step of the view work they left.
    let session = Session {
        ops: &ops,
        who,
        redactor: &redactor,
        reach: Reach::Checked,
    };
    let served = reply_then_step(
        || session.serve(&mut input, &mut output),
        || step_store(&path),
        &redactor,
        &mut io::stderr(),
    );
    Ok(served)
}

/// Steps 1 to 3 of [the module documentation](self): the store `name` under the root, of
/// FORMAT 2 or later, which `who` may read; its path and its operations.
fn open(
    invocation: &ServeInvocation,
    who: &Identity,
    name: &str,
) -> Result<(camino::Utf8PathBuf, LocalOps), OpError> {
    // 1. The store under the root.
    let path = store_under(&invocation.root, name)?;
    let ops = LocalOps::open(&path)?;

    // 2. Its format: FORMAT 1 has no tracking refs, branches or tags.
    let format = ops.repo.format();
    if format == StoreFormat::V1 {
        let needs = StoreError::FormatRequired {
            name: name.to_owned(),
            needs: StoreFormat::V2.version(),
            store: format.version(),
        };
        return Err(error::op(RepoError::from(needs)));
    }

    // 3. The caller may read it.
    Access::load(&ops)?.check(who, Action::Read, name)?;
    Ok((path, ops))
}

/// Writes `e` as the session's one answer line and ends it with `e`'s exit code.
fn refuse_session(output: &mut impl Write, e: OpError, redactor: &Redactor) -> Served {
    let exit = e.exit;
    let _ = answer(output, SESSION_OP, Err(e), Instant::now(), redactor);
    Served::new(exit)
}

/// Whether `wire_get` checks that a ref reaches what it sends (V9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reach {
    /// Every object asked for is reached from the ref named, else answered as absent.
    Checked,
    /// No check: the self-check of the reach tests, which then see an unreached object sent.
    #[cfg(test)]
    Unchecked,
}

/// Whether the session reads on after an answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    /// The next line follows.
    Go,
    /// The input is past a point the wire can read on from: the session ends.
    Stop,
}

/// What a handler answered: the result, and the chunks of objects that follow its line.
struct Handled {
    /// The op the answer names.
    op: String,
    /// The result, or the refusal.
    outcome: Result<Value, OpError>,
    /// The objects of each chunk that follows the line, in order.
    chunks: Vec<Vec<ObjectId>>,
    /// Whether the session reads on.
    end: End,
}

impl Handled {
    /// The answer `outcome` to `op`, with no chunk.
    fn line(op: &str, outcome: Result<Value, OpError>, end: End) -> Handled {
        Handled {
            op: op.to_owned(),
            outcome,
            chunks: Vec::new(),
            end,
        }
    }
}

/// One session: the store, its caller, and how its answers are masked.
struct Session<'a> {
    ops: &'a LocalOps,
    who: &'a Identity,
    redactor: &'a Redactor,
    reach: Reach,
}

impl Session<'_> {
    /// Answers each line of `input` on `output` until `input` ends (exit 0) or a refusal ends
    /// the session (its exit code).
    fn serve(&self, input: &mut impl BufRead, output: &mut impl Write) -> Served {
        loop {
            // 1. One line, bounded; the end of the input ends the session.
            let started = Instant::now();
            let line = match read_line(input) {
                Ok(Some(line)) => line,
                Ok(None) => return Served::new(0),
                Err(e) => return refuse_session(output, e, self.redactor),
            };

            // 2. Its answer, then the chunks it announces.
            let handled = self.handle(&line, input);
            let exit = handled.outcome.as_ref().err().map_or(0, |e| e.exit);
            let ok = handled.outcome.is_ok();
            let written = answer(output, &handled.op, handled.outcome, started, self.redactor);
            if written.is_err() {
                return Served::new(ErrorCode::RemoteUnreachable.exit());
            }
            if ok && let Err(e) = self.send(output, &handled.chunks) {
                return Served::new(e.exit);
            }
            if handled.end == End::Stop {
                return Served::new(exit);
            }
        }
    }

    /// The answer to the request `line`, whose chunks, if it is a `wire_put`, `input` holds.
    fn handle(&self, line: &[u8], input: &mut impl BufRead) -> Handled {
        // 1. A JSON object, else the input is no wire: the session ends.
        let Ok(Value::Object(mut request)) = serde_json::from_slice::<Value>(line) else {
            let e = invalid("bad_request", "a request line that is no JSON object");
            return Handled::line(SESSION_OP, Err(e), End::Stop);
        };
        let op = match request.get("op").and_then(Value::as_str) {
            Some(op) => op.to_owned(),
            None => SESSION_OP.to_owned(),
        };

        // 2. Its API version and protocol, before its body: a put's chunks are then unread.
        let unread = if op == WIRE_PUT { End::Stop } else { End::Go };
        if let Err(e) = check_header(&op, &request) {
            return Handled::line(&op, Err(e), unread);
        }
        let req = request.remove("req").unwrap_or(Value::Null);

        // 3. The op's handler.
        let outcome = match op.as_str() {
            WIRE_HELLO => parse(req).and_then(|req| self.hello(&req)),
            WIRE_REFS => parse(req).and_then(|req| self.refs(&req)),
            WIRE_LIST => parse(req).and_then(|req| self.list(&req)),
            WIRE_GET => {
                let got = parse(req).and_then(|req| self.get(&req));
                return match got {
                    Ok((announced, chunks)) => Handled {
                        op,
                        outcome: to_value(&announced),
                        chunks,
                        end: End::Go,
                    },
                    Err(e) => Handled::line(WIRE_GET, Err(e), End::Go),
                };
            }
            WIRE_HAS => parse(req).and_then(|req| self.has(&req)),
            WIRE_PUT => {
                let (outcome, end) = self.put(req, input);
                return Handled::line(WIRE_PUT, outcome, end);
            }
            WIRE_LAND => parse(req).and_then(|req| self.land(&req)),
            WIRE_MOVE => parse(req).and_then(|req| self.wire_move(&req)),
            _ => Err(invalid(
                "unknown_op",
                format!("{op} is no op this server answers"),
            )),
        };
        Handled::line(&op, outcome, End::Go)
    }

    /// `wire_hello`: the highest protocol both sides take, and who the caller is.
    fn hello(&self, req: &WireHelloReq) -> Result<Value, OpError> {
        let takes = req.protocol_min <= WIRE_PROTOCOL && WIRE_PROTOCOL <= req.protocol_max;
        if !takes {
            let message = format!(
                "this server speaks protocol {WIRE_PROTOCOL}, not {} to {}",
                req.protocol_min, req.protocol_max
            );
            return Err(protocol_refused(message));
        }
        let hello = WireHello::new(
            WIRE_PROTOCOL,
            self.who.agent(),
            self.ops.repo.format().version(),
            OBJECT_KINDS.map(str::to_owned).to_vec(),
            Vec::new(),
            env!("CARGO_PKG_VERSION"),
        );
        to_value(&hello)
    }

    /// `wire_refs`: a page of the refs a host may fetch, in name order; remote-tracking refs
    /// are the server's own and are not listed.
    fn refs(&self, req: &WireRefsReq) -> Result<Value, OpError> {
        check_limit(req.limit)?;
        let read = self.snapshot()?;
        let listed = listed_refs(&read)?;
        let has_main = listed.iter().any(|(name, _)| name == "main");

        // 1. The page after `after`, within its count and its bytes.
        let after = req.after.as_deref();
        let rest = (listed.into_iter()).filter(|(name, _)| after.is_none_or(|a| name.as_str() > a));
        let mut refs = Vec::new();
        let mut used = 0;
        let mut more = false;
        for (name, target) in rest {
            let cost = name.len() + 2 * 32 + REF_JSON_BYTES;
            if refs.len() == req.limit || used + cost > REFS_PAGE_BYTES {
                more = true;
                break;
            }
            used += cost;
            refs.push(WireRef::new(name, target.to_string()));
        }

        // 2. Where the next page starts.
        let mut page = WireRefs::new(refs, has_main);
        if more {
            page.next = page.refs.last().map(|last| last.name.clone());
        }
        to_value(&page)
    }

    /// `wire_list`: a page of what `want`, a listed ref's head, reaches and no `have` does.
    fn list(&self, req: &WireListReq) -> Result<Value, OpError> {
        check_limit(req.limit)?;
        check_count(req.have.len())?;
        let want = changeset_id(&req.want, "want")?;
        let have = (req.have.iter())
            .map(|id| changeset_id(id, "have"))
            .collect::<Result<Vec<_>, _>>()?;
        let after = match &req.after {
            Some(cursor) => Some((cursor.generation, changeset_id(&cursor.id, "after")?)),
            None => None,
        };

        // 1. `want` is a listed ref's head now, else the client reads refs again.
        let read = self.snapshot()?;
        let heads = listed_refs(&read)?;
        if !heads.iter().any(|(_, target)| *target == want) {
            let message = format!("{want} is the head of no ref");
            return Err(refused(ErrorCode::RevNotFound, "not_a_ref_head", message)
                .with_hint("read the refs again (wire_refs)"));
        }

        // 2. The page.
        let missing = read.missing(&have, want, req.limit, after);
        let missing = missing.map_err(error::op)?;
        let ids = missing.changesets.iter().map(ToString::to_string).collect();
        let mut page = WireList::new(ids);
        page.next =
            (missing.next).map(|(generation, id)| WireCursor::new(generation, id.to_string()));
        to_value(&page)
    }

    /// `wire_get`: the chunks announcement and the objects of each chunk, after V9: every
    /// changeset asked for reached from `under`'s head, every blob named by the delta of a
    /// changeset so reached; anything else answered as an object the store lacks.
    fn get(&self, req: &WireGetReq) -> Result<(WireChunks, Vec<Vec<ObjectId>>), OpError> {
        check_count(req.changesets.len() + req.blobs.len())?;
        let changesets = (req.changesets.iter())
            .map(|id| changeset_id(id, "changesets"))
            .collect::<Result<Vec<_>, _>>()?;
        let mut blobs = Vec::with_capacity(req.blobs.len());
        for want in &req.blobs {
            blobs.push((changeset_id(&want.of, "of")?, object_id(&want.id, "blobs")?));
        }

        // 1. Every changeset asked for, and every blob's `of`, reached from `under`.
        let read = self.snapshot()?;
        let head = ref_head(&read, &req.under)?;
        let asked: Vec<ChangesetId> = (changesets.iter().copied())
            .chain(blobs.iter().map(|(of, _)| *of))
            .collect();
        if let Some(lost) = self.unreached(&read, head, &asked)? {
            return Err(not_served(lost.0, &req.under));
        }

        // 2. Every blob named by its `of`'s delta.
        let mut deltas: HashMap<ChangesetId, HashSet<BlobId>> = HashMap::new();
        for (of, blob) in &blobs {
            if !deltas.contains_key(of) {
                deltas.insert(*of, puts_of(&read, *of)?);
            }
            let named = deltas
                .get(of)
                .is_some_and(|puts| puts.contains(&BlobId::new(*blob)));
            if !named && self.reach == Reach::Checked {
                return Err(not_served(*blob, &req.under));
            }
        }

        // 3. Each object's length, then the chunks: many records within a chunk's bound, one
        //    alone past it.
        let mut sized = Vec::with_capacity(asked.len());
        for id in &changesets {
            sized.push((
                id.0,
                self.len_of(&read, id.0, ObjectKind::Changeset, &req.under)?,
            ));
        }
        for (_, blob) in &blobs {
            sized.push((
                *blob,
                self.len_of(&read, *blob, ObjectKind::Blob, &req.under)?,
            ));
        }
        Ok(plan_chunks(&sized))
    }

    /// `wire_has`: which ids the store holds, an adopted changeset or a blob.
    fn has(&self, req: &WireHasReq) -> Result<Value, OpError> {
        Access::load(self.ops)?.check_upload(self.who)?;
        check_count(req.ids.len())?;
        let read = self.snapshot()?;
        let mut held = Vec::with_capacity(req.ids.len());
        for id in &req.ids {
            held.push(holds(&read, object_id(id, "ids")?)?);
        }
        to_value(&WireHas::new(held))
    }

    /// `wire_put`: the chunks that follow `req`'s line read, their blobs stored as they come,
    /// then the changesets named in `adopt` adopted, no ref moved. A refusal before the last
    /// chunk is read ends the session.
    fn put(&self, req: Value, input: &mut impl BufRead) -> (Result<Value, OpError>, End) {
        // 1. Everything a refusal may come from before the chunks are read.
        let checked = parse::<WirePutReq>(req).and_then(|req| self.check_put(&req).map(|()| req));
        let req = match checked {
            Ok(req) => req,
            Err(e) => return (Err(e), End::Stop),
        };

        // 2. The chunks: blobs stored as they come, changesets kept for the adopt.
        let received = self.receive(&req, input);
        let received = match received {
            Ok(received) => received,
            Err(e) => return (Err(e), End::Stop),
        };

        // 3. The adopt, the input read to its end.
        (self.adopt(&req, received), End::Go)
    }

    /// The checks of a put before its chunks: the caller uploads, the announcement is within
    /// [`MAX_PUSH_BYTES`] and consistent, the ids to adopt are ids.
    fn check_put(&self, req: &WirePutReq) -> Result<(), OpError> {
        Access::load(self.ops)?.check_upload(self.who)?;
        if req.bytes > MAX_PUSH_BYTES {
            let message = format!(
                "a put of {} bytes, past {MAX_PUSH_BYTES}: split it over requests",
                req.bytes
            );
            let mut e = invalid("too_large", message);
            e.details.insert("bytes".to_owned(), req.bytes.into());
            e.details.insert("limit".to_owned(), MAX_PUSH_BYTES.into());
            return Err(e);
        }
        if (req.chunks == 0) != (req.bytes == 0) {
            return Err(invalid(
                "chunk_count",
                "a put's chunks and bytes do not agree",
            ));
        }
        check_count(req.adopt.len())?;
        for id in &req.adopt {
            changeset_id(id, "adopt")?;
        }
        Ok(())
    }

    /// The chunks of a put read from `input`, each checked as it is read (V1); its blobs stored
    /// at once, its changesets returned by id.
    fn receive(
        &self,
        req: &WirePutReq,
        input: &mut impl BufRead,
    ) -> Result<HashMap<ChangesetId, Hashed>, OpError> {
        let mut left = WireChunks::new(req.chunks, req.bytes);
        let mut changesets = HashMap::new();
        while left.chunks > 0 {
            let records = chunk::read_chunk(input, left, Takes::BlobsAndChangesets)?;
            let len = chunk::chunk_len(&records);
            left = WireChunks::new(left.chunks - 1, left.bytes - len);
            let (blobs, others): (Vec<Hashed>, Vec<Hashed>) =
                (records.into_iter()).partition(|record| record.kind() == ObjectKind::Blob);
            if !blobs.is_empty() {
                self.ops.repo.put_blobs(blobs).map_err(error::op)?;
            }
            for changeset in others {
                changesets.insert(ChangesetId::new(changeset.id()), changeset);
            }
        }
        Ok(changesets)
    }

    /// The adopt of `req.adopt` from the changesets `received`: one held already and not sent
    /// is present; one neither sent nor held is refused before anything is adopted.
    fn adopt(
        &self,
        req: &WirePutReq,
        mut received: HashMap<ChangesetId, Hashed>,
    ) -> Result<Value, OpError> {
        // 1. Each id to adopt: sent, or held.
        let read = self.snapshot()?;
        let mut adopting = Vec::with_capacity(req.adopt.len());
        let mut held = Vec::new();
        for text in &req.adopt {
            let id = changeset_id(text, "adopt")?;
            if let Some(object) = received.remove(&id) {
                adopting.push(object);
                continue;
            }
            if read.changeset(id).map_err(error::op)?.is_none() {
                let message = format!("{id} is to be adopted, and the put sent no such changeset");
                return Err(invalid("not_sent", message));
            }
            held.push(id);
        }
        drop(read);

        // 2. The adopt, parents first as given.
        let adopted = self.ops.repo.adopt(adopting).map_err(error::op)?;
        let ids = |ids: &[ChangesetId]| ids.iter().map(ToString::to_string).collect::<Vec<_>>();
        let mut present = ids(&adopted.present);
        present.extend(ids(&held));
        to_value(&WirePut::new(ids(&adopted.adopted), present))
    }

    /// `wire_land`: the change `head` adds after `base` landed on `target`, keyed by
    /// [`sync_key`] of the target and the head, `access.toml` read again before each attempt.
    fn land(&self, req: &WireLandReq) -> Result<Value, OpError> {
        let head = changeset_id(&req.head, "head")?;
        let base = changeset_id(&req.base, "base")?;
        let target = req.target.as_str();
        Access::load(self.ops)?.check(self.who, Action::Land, target)?;

        // 1. The land, keyed by which change lands where.
        let mut land = LandReq::new(Rev::from(head.to_string()));
        land.base = Some(Rev::from(base.to_string()));
        target.clone_into(&mut land.target);
        land.idempotency_key = Some(sync_key(target, head));
        land.max_attempts = req.max_attempts;
        land.max_conflict_bytes = req.max_conflict_bytes.min(MAX_NATIVE_CONFLICT_BYTES);

        // 2. Each attempt gated on the grant as access.toml gives it then (V7).
        let granted = |_attempt: u32| Access::load(self.ops)?.check(self.who, Action::Land, target);
        let landed = self.ops.land_gated(self.who, land, granted)?;
        to_value(&landed)
    }

    /// `wire_move`: one ref moved by compare-and-swap under the store's ref rules (V10).
    fn wire_move(&self, req: &WireMoveReq) -> Result<Value, OpError> {
        let name = req.name.as_str();
        let old = optional_id(req.old.as_deref(), "old")?;
        let new = optional_id(req.new.as_deref(), "new")?;
        if old.is_none() && new.is_none() {
            return Err(invalid(
                "nothing_to_move",
                "a move names neither old nor new",
            ));
        }

        // 1. The rules of the ref's kind, whatever the grants: main and release/* never forced
        //    or deleted, a tag created only.
        let kind = ref_kind(name);
        let protected = matches!(kind, Some(RefKind::Main | RefKind::Release));
        if protected && (new.is_none() || req.mode == WireMoveMode::Force) {
            let e = StoreError::RefProtected(name.to_owned());
            return Err(error::op(RepoError::from(e)));
        }
        if kind == Some(RefKind::Tag) && old.is_some() {
            let e = StoreError::TagImmutable(name.to_owned());
            return Err(error::op(RepoError::from(e)));
        }

        // 2. The grant the move needs, read again (V7); another branch forward unless forced.
        let action = match (new, req.mode) {
            (None, _) => Action::Delete,
            (Some(_), WireMoveMode::Force) => Action::Force,
            (Some(_), _) => Action::Push,
        };
        Access::load(self.ops)?.check(self.who, action, name)?;
        if let Some(new) = new {
            self.check_new(name, kind, old, new, req.mode)?;
        }

        // 3. The move, through ref set and ref delete.
        let moved = match (new, old) {
            (Some(new), old) => {
                let mut set = RefSetReq::new(name, Rev::from(new.to_string()));
                match old {
                    Some(old) => set.expected_old = Some(Rev::from(old.to_string())),
                    None => set.create = true,
                }
                self.ops.ref_set(self.who, set)?
            }
            (None, Some(old)) => {
                let delete = RefDeleteReq::new(name, Rev::from(old.to_string()));
                self.ops.ref_delete(self.who, delete)?
            }
            (None, None) => return Err(error::unknown_variant("WireMoveReq")),
        };
        to_value(&WireMove::new(moved.name, moved.old, moved.new))
    }

    /// A move of `name`, of kind `kind`, from `old` to `new` by `mode`: `new` is a changeset the
    /// store holds, and another branch's advance holds `old` in its history.
    fn check_new(
        &self,
        name: &str,
        kind: Option<RefKind>,
        old: Option<ChangesetId>,
        new: ChangesetId,
        mode: WireMoveMode,
    ) -> Result<(), OpError> {
        let read = self.snapshot()?;
        if read.changeset(new).map_err(error::op)?.is_none() {
            let message = format!("{new} is no changeset the store holds: put it first");
            return Err(refused(ErrorCode::RevNotFound, "not_held", message));
        }
        let Some(old) = old else {
            return Ok(());
        };
        if kind != Some(RefKind::Branch) || mode == WireMoveMode::Force {
            return Ok(());
        }
        if read.changeset(old).map_err(error::op)?.is_none() {
            return Ok(());
        }
        let ancestry = read.ancestry(old, new, 0, None).map_err(error::op)?;
        if ancestry.is_ancestor {
            return Ok(());
        }
        let message = format!("{name} moves forward alone: {old} is not in {new}'s history");
        Err(invalid("not_forward", message).with_hint("move it with mode force"))
    }

    /// The length of object `id` of `kind` as a chunk record carries it; one the store lacks,
    /// or holds as another kind, is not served under `under`.
    fn len_of(
        &self,
        read: &Snapshot<'_>,
        id: ObjectId,
        kind: ObjectKind,
        under: &str,
    ) -> Result<u64, OpError> {
        let len = match kind {
            ObjectKind::Blob => match read.blob_len(BlobId::new(id)) {
                Ok(len) => Some(len),
                Err(RepoError::MissingObject { .. }) => None,
                Err(e) => return Err(error::op(e)),
            },
            _ => match self.ops.repo.object(id).map_err(error::op)? {
                Some((found, payload)) if found == kind => Some(bytes::to_u64(payload.len())),
                _ => None,
            },
        };
        len.ok_or_else(|| not_served(id, under))
    }

    /// The first changeset of `asked`, in its order, that the store lacks or `head` does not
    /// reach: one walk from `head`, a page at a time, down to the lowest generation asked for.
    fn unreached(
        &self,
        read: &Snapshot<'_>,
        head: ChangesetId,
        asked: &[ChangesetId],
    ) -> Result<Option<ChangesetId>, OpError> {
        // 1. Each one held, and the lowest generation among them.
        let mut floor = u64::MAX;
        for &id in asked {
            let Some(stored) = read.changeset(id).map_err(error::op)? else {
                return Ok(Some(id));
            };
            floor = floor.min(stored.generation);
        }
        if self.reach != Reach::Checked || asked.is_empty() {
            return Ok(None);
        }

        // 2. The walk, until each is met or the walk is below them all.
        let mut left: HashSet<ChangesetId> = asked.iter().copied().collect();
        let mut after = None;
        loop {
            let page = read.missing(&[], head, MAX_WIRE_IDS, after);
            let page = page.map_err(error::op)?;
            for id in &page.changesets {
                left.remove(id);
            }
            match page.next {
                _ if left.is_empty() => return Ok(None),
                Some((generation, id)) if generation >= floor => after = Some((generation, id)),
                _ => break,
            }
        }
        Ok(asked.iter().copied().find(|id| left.contains(id)))
    }

    /// Writes the chunks of `plan` to `output`, each object read again as it goes.
    fn send(&self, output: &mut impl Write, plan: &[Vec<ObjectId>]) -> Result<(), OpError> {
        for ids in plan {
            // 1. The chunk's objects, read; one gone since its length was read ends the session.
            let mut objects = Vec::with_capacity(ids.len());
            for &id in ids {
                let Some((kind, payload)) = self.ops.repo.object(id).map_err(error::op)? else {
                    return Err(not_served(id, "the ref"));
                };
                objects.push((kind, id, payload));
            }

            // 2. The chunk.
            let records: Vec<(ObjectKind, ObjectId, &[u8])> = (objects.iter())
                .map(|(kind, id, payload)| (*kind, *id, payload.as_slice()))
                .collect();
            let lost = |e: io::Error| crate::native::unreachable(crate::native::Cause::of(&e));
            chunk::write_chunk(output, &records).map_err(lost)?;
        }
        output
            .flush()
            .map_err(|e| crate::native::unreachable(crate::native::Cause::of(&e)))
    }

    /// A snapshot of the store.
    fn snapshot(&self) -> Result<Snapshot<'_>, OpError> {
        self.ops.repo.snapshot().map_err(error::op)
    }
}

/// One line of `input` without its newline, at most [`MAX_WIRE_LINE`] bytes; `None` at the end
/// of the input. A longer line, or one the input ends inside, is refused.
fn read_line(input: &mut impl BufRead) -> Result<Option<Vec<u8>>, OpError> {
    let mut line = Vec::new();
    let bound = bytes::to_u64(MAX_WIRE_LINE) + 1;
    let read = input.by_ref().take(bound).read_until(NEWLINE, &mut line);
    let read = read.map_err(|e| crate::native::unreachable(crate::native::Cause::of(&e)))?;
    if read == 0 {
        return Ok(None);
    }
    if line.last() == Some(&NEWLINE) {
        line.pop();
        return Ok(Some(line));
    }
    if line.len() > MAX_WIRE_LINE {
        let message = format!("a request line past {MAX_WIRE_LINE} bytes");
        return Err(invalid("line_too_long", message));
    }
    Err(invalid(
        "truncated",
        "the input ended inside a request line",
    ))
}

/// Writes the envelope of `outcome` to `op` as one line, its error masked by `redactor`.
fn answer(
    output: &mut impl Write,
    op: &str,
    outcome: Result<Value, OpError>,
    started: Instant,
    redactor: &Redactor,
) -> io::Result<()> {
    let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let meta = Meta::new(elapsed, BTreeMap::new(), AUTH_TRUST);
    let envelope = match outcome {
        Ok(result) => Envelope::ok(op, result, meta),
        Err(e) => Envelope::err(op, redactor.redact_error(e), meta),
    };
    serde_json::to_writer(&mut *output, &envelope)?;
    output.write_all(&[NEWLINE])?;
    output.flush()
}

/// The checks of a request's header: its API version, and, but for `wire_hello`, which names
/// its range in its body, its protocol.
fn check_header(op: &str, request: &Map<String, Value>) -> Result<(), OpError> {
    let api = request.get("api").and_then(Value::as_u64);
    if api != Some(u64::from(API_VERSION)) {
        let message = format!("a request of API version {api:?}: this server reads {API_VERSION}");
        return Err(invalid("api", message));
    }
    if op == WIRE_HELLO {
        return Ok(());
    }
    let protocol = request.get("protocol").and_then(Value::as_u64);
    if protocol != Some(u64::from(WIRE_PROTOCOL)) {
        let message =
            format!("a request in protocol {protocol:?}: this server speaks {WIRE_PROTOCOL}");
        return Err(protocol_refused(message));
    }
    Ok(())
}

/// The refs a host may fetch, by name: every ref but the remote-tracking ones.
fn listed_refs(read: &Snapshot<'_>) -> Result<Vec<(String, ChangesetId)>, OpError> {
    let refs = read.list_refs().map_err(error::op)?;
    let listed = (refs.into_iter())
        .filter(|(name, _)| !matches!(ref_kind(name), Some(RefKind::RemoteTracking) | None))
        .map(|(name, state)| (name, state.target))
        .collect();
    Ok(listed)
}

/// The head of `under`, a ref [`listed_refs`] lists; else `E_REV_NOT_FOUND` `not_a_ref`.
fn ref_head(read: &Snapshot<'_>, under: &str) -> Result<ChangesetId, OpError> {
    let listed = !matches!(ref_kind(under), Some(RefKind::RemoteTracking) | None);
    let state = if listed {
        read.get_ref(under).map_err(error::op)?
    } else {
        None
    };
    let Some(state) = state else {
        let message = format!("{under} is no ref this server lists");
        return Err(refused(ErrorCode::RevNotFound, "not_a_ref", message)
            .with_hint("read the refs again (wire_refs)"));
    };
    Ok(state.target)
}

/// The blobs changeset `of`'s delta puts.
fn puts_of(read: &Snapshot<'_>, of: ChangesetId) -> Result<HashSet<BlobId>, OpError> {
    let Some(stored) = read.changeset(of).map_err(error::op)? else {
        return Ok(HashSet::new());
    };
    let puts = (stored.changeset.delta.iter())
        .filter_map(|entry| match entry.op {
            DeltaOp::Put { id, .. } => Some(id),
            _ => None,
        })
        .collect();
    Ok(puts)
}

/// Whether the store holds `id` for `wire_has`: an adopted changeset, or a blob.
fn holds(read: &Snapshot<'_>, id: ObjectId) -> Result<bool, OpError> {
    if read
        .changeset(ChangesetId::new(id))
        .map_err(error::op)?
        .is_some()
    {
        return Ok(true);
    }
    match read.blob_len(BlobId::new(id)) {
        Ok(_) => Ok(true),
        Err(RepoError::MissingObject { .. }) => Ok(false),
        Err(e) => Err(error::op(e)),
    }
}

/// The announcement and the chunks of the objects `sized`, each with its length: records added
/// to a chunk while its bytes stay within [`MAX_CHUNK_BYTES`], a record past it alone.
fn plan_chunks(sized: &[(ObjectId, u64)]) -> (WireChunks, Vec<Vec<ObjectId>>) {
    let mut chunks: Vec<Vec<ObjectId>> = Vec::new();
    let mut total = 0;
    let mut used = MAX_CHUNK_BYTES;
    for &(id, len) in sized {
        let record = RECORD_HEADER_LEN + len;
        if chunks.is_empty() || used + record > MAX_CHUNK_BYTES {
            chunks.push(Vec::new());
            total += HEADER_LEN;
            used = 0;
        }
        if let Some(last) = chunks.last_mut() {
            last.push(id);
        }
        used += record;
        total += record;
    }
    let count = u32::try_from(chunks.len()).unwrap_or(u32::MAX);
    (WireChunks::new(count, total), chunks)
}

/// `E_INVALID_ARG` with `details.reason`.
fn invalid(reason: &str, message: impl Into<String>) -> OpError {
    refused(ErrorCode::InvalidArg, reason, message)
}

/// `E_INVALID_ARG` `protocol`: a protocol this server does not speak.
fn protocol_refused(message: String) -> OpError {
    let mut e = invalid("protocol", message);
    e.details
        .insert("protocol".to_owned(), WIRE_PROTOCOL.into());
    e
}

/// `E_REV_NOT_FOUND` for object `id` under the ref `under`: the same answer whether the store
/// lacks it or no listed ref reaches it.
fn not_served(id: ObjectId, under: &str) -> OpError {
    let message = format!("{id} is not served under {under}");
    let mut e = refused(ErrorCode::RevNotFound, "not_found", message);
    e.details.insert("id".to_owned(), id.to_string().into());
    e
}

/// A `limit` within 1 and [`MAX_WIRE_IDS`].
fn check_limit(limit: usize) -> Result<(), OpError> {
    if (1..=MAX_WIRE_IDS).contains(&limit) {
        return Ok(());
    }
    Err(invalid(
        "limit",
        format!("a limit of {limit}, not 1 to {MAX_WIRE_IDS}"),
    ))
}

/// A list of at most [`MAX_WIRE_IDS`] ids.
fn check_count(count: usize) -> Result<(), OpError> {
    if count <= MAX_WIRE_IDS {
        return Ok(());
    }
    Err(invalid(
        "too_many_ids",
        format!("{count} ids, past {MAX_WIRE_IDS}"),
    ))
}

/// The id `text` in the request's `field`: 64 lowercase hex digits.
fn object_id(text: &str, field: &str) -> Result<ObjectId, OpError> {
    let id = text
        .parse::<ObjectId>()
        .ok()
        .filter(|id| id.to_hex() == text);
    id.ok_or_else(|| {
        let mut e = invalid("bad_id", format!("{field}: {text:?} is no id"));
        e.details.insert("field".to_owned(), field.into());
        e
    })
}

/// The changeset id `text` in the request's `field`.
fn changeset_id(text: &str, field: &str) -> Result<ChangesetId, OpError> {
    object_id(text, field).map(ChangesetId::new)
}

/// The changeset id `text`, if given, in the request's `field`.
fn optional_id(text: Option<&str>, field: &str) -> Result<Option<ChangesetId>, OpError> {
    text.map(|text| changeset_id(text, field)).transpose()
}

/// `req` read as the op's request, else `E_INVALID_ARG` `bad_request`.
fn parse<T: DeserializeOwned>(req: Value) -> Result<T, OpError> {
    serde_json::from_value(req).map_err(|e| invalid("bad_request", error::one_line(e.to_string())))
}

/// `result` as JSON.
fn to_value(result: &impl Serialize) -> Result<Value, OpError> {
    serde_json::to_value(result).map_err(|e| error::error(ErrorCode::Internal, e.to_string()))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use camino::Utf8PathBuf;
    use serde_json::json;
    use swarmops_api::{ChangeInput, ChangeOp, CommitReq, ContentInput, Rev, ShowReq};

    use super::*;

    /// The caller every test serves.
    const AGENT: &str = "agent/worker-17";

    /// A store served to [`AGENT`], who may read, push and land everywhere.
    struct Site {
        _dir: tempfile::TempDir,
        ops: LocalOps,
        who: Identity,
    }

    impl Site {
        /// A FORMAT 2 store whose `access.toml` grants [`AGENT`] read, push and land.
        fn new() -> Site {
            let dir = tempfile::tempdir().unwrap();
            let root = Utf8PathBuf::from_path_buf(dir.path().join("acme")).unwrap();
            let (ops, _) = LocalOps::init(&root, "acme").unwrap();
            let access = "version = 1\n\n[[grant]]\nwho = [\"agent/*\"]\npush = [\"**\"]\n\
                          land = [\"main\"]\n";
            std::fs::write(root.join("access.toml"), access).unwrap();
            let who = Identity::parse(AGENT).unwrap();
            Site {
                _dir: dir,
                ops,
                who,
            }
        }

        /// Commits a put of `path` on `parent`, advancing `advance`; the changeset's id.
        fn commit(&self, parent: Option<&str>, path: &str, advance: Option<&str>) -> String {
            let mut change = ChangeInput::new(path.to_owned(), ChangeOp::Put);
            change.content = Some(ContentInput::Inline(format!("{path}\n")));
            let mut req = CommitReq::new(vec![change], path.to_owned());
            req.parent = parent.map(Rev::from);
            req.advance = advance.map(str::to_owned);
            self.ops.commit(&self.who, req).unwrap().changeset
        }

        /// The blob changeset `id`'s delta puts first.
        fn blob_of(&self, id: &str) -> String {
            let id = changeset_id(id, "id").unwrap();
            let puts = puts_of(&self.ops.repo.snapshot().unwrap(), id).unwrap();
            puts.into_iter().next().unwrap().to_string()
        }

        /// The answers to `lines`, each a request, under `reach`: each answer's envelope, the
        /// chunk bytes after it skipped.
        fn ask(&self, reach: Reach, lines: &[Value]) -> Vec<Value> {
            let mut input = Vec::new();
            for line in lines {
                input.extend(serde_json::to_vec(line).unwrap());
                input.push(NEWLINE);
            }
            let session = Session {
                ops: &self.ops,
                who: &self.who,
                redactor: &Redactor::new(&[]),
                reach,
            };
            let mut output = Vec::new();
            session.serve(&mut Cursor::new(input), &mut output);
            envelopes(&output)
        }
    }

    /// The envelopes of `output`, each line's announced chunk bytes skipped.
    fn envelopes(mut output: &[u8]) -> Vec<Value> {
        let mut out = Vec::new();
        while let Some(end) = output.iter().position(|&b| b == NEWLINE) {
            let envelope: Value = serde_json::from_slice(&output[..end]).unwrap();
            let skip = envelope["result"]["bytes"].as_u64().unwrap_or(0);
            output = &output[end + 1 + usize::try_from(skip).unwrap()..];
            out.push(envelope);
        }
        out
    }

    /// A request line of `op` with `req`.
    fn request(op: &str, req: &Value) -> Value {
        json!({"op": op, "api": API_VERSION, "protocol": WIRE_PROTOCOL, "req": req})
    }

    /// A store with `main` at `a`, and `b`, a child of `a` no ref reaches: `(a, b)`.
    fn unreached(served: &Site) -> (String, String) {
        let a = served.commit(None, "a", Some("main"));
        let b = served.commit(Some(&a), "b", None);
        (a, b)
    }

    /// Asserts `envelope` refuses `id` as not served under `under`: the answer an id the store
    /// lacks gets.
    fn assert_not_served(envelope: &Value, id: &str, under: &str) {
        assert_eq!(envelope["ok"], false, "{envelope}");
        let error = &envelope["error"];
        assert_eq!(error["code"], "E_REV_NOT_FOUND", "{envelope}");
        assert_eq!(error["details"]["reason"], "not_found", "{envelope}");
        assert_eq!(error["details"]["id"], id, "{envelope}");
        assert_eq!(
            error["message"],
            format!("{id} is not served under {under}")
        );
    }

    /// The reach tests' first check, under `reach`: a changeset no ref reaches is answered as
    /// one the store lacks, while `main`'s own is sent.
    fn unreached_changeset_answers(reach: Reach) {
        let served = Site::new();
        let (a, b) = unreached(&served);
        let absent = "ab".repeat(32);
        let get = |id: &str| request(WIRE_GET, &json!({"under": "main", "changesets": [id]}));
        let answers = served.ask(reach, &[get(&a), get(&b), get(&absent)]);
        assert_eq!(answers[0]["result"]["chunks"], 1, "{}", answers[0]);
        assert_not_served(&answers[1], &b, "main");
        assert_not_served(&answers[2], &absent, "main");
    }

    /// V9: a changeset that no ref reaches is not sent: the store answers it as one it lacks.
    #[test]
    fn an_object_no_ref_reaches_is_not_sent() {
        unreached_changeset_answers(Reach::Checked);
    }

    /// The self-check of the reach tests: without the reach check, the unreached changeset is
    /// sent, so the test above fails.
    #[test]
    fn without_the_reach_check_the_unreached_object_is_sent() {
        let unchecked = std::panic::catch_unwind(|| unreached_changeset_answers(Reach::Unchecked));
        assert!(
            unchecked.is_err(),
            "the reach test passes with no reach check"
        );

        // And the object is indeed sent.
        let served = Site::new();
        let (_, b) = unreached(&served);
        let get = request(WIRE_GET, &json!({"under": "main", "changesets": [b]}));
        let answers = served.ask(Reach::Unchecked, &[get]);
        assert_eq!(answers[0]["result"]["chunks"], 1, "{}", answers[0]);
    }

    /// V9: a blob is sent only when the delta of a changeset a ref reaches names it.
    #[test]
    fn a_blob_no_reached_delta_names_is_not_sent() {
        let served = Site::new();
        let (a, b) = unreached(&served);
        let (blob_a, blob_b) = (served.blob_of(&a), served.blob_of(&b));
        let get = |of: &str, id: &str| {
            request(
                WIRE_GET,
                &json!({"under": "main", "blobs": [{"of": of, "id": id}]}),
            )
        };
        let answers = served.ask(
            Reach::Checked,
            &[get(&a, &blob_a), get(&a, &blob_b), get(&b, &blob_b)],
        );
        assert_eq!(answers[0]["result"]["chunks"], 1, "{}", answers[0]);
        assert_not_served(&answers[1], &blob_b, "main");
        assert_not_served(&answers[2], &b, "main");
    }

    /// A land is keyed by `sync_key` of its target and head: the landed changeset carries it,
    /// and the same land again replays.
    #[test]
    fn a_land_derives_its_key_from_the_change() {
        // 1. main at a; a change b on a, which no ref names.
        let served = Site::new();
        let (a, b) = unreached(&served);
        let land = request(WIRE_LAND, &json!({"target": "main", "base": a, "head": b}));
        let answers = served.ask(Reach::Checked, &[land.clone(), land]);

        // 2. Landed, then replayed onto the same changeset.
        let landed = &answers[0]["result"];
        assert_eq!(landed["status"], "landed", "{}", answers[0]);
        assert_eq!(answers[1]["result"]["status"], "replayed", "{}", answers[1]);
        assert_eq!(answers[1]["result"]["landed"], landed["landed"]);

        // 3. The landed changeset's key.
        let shown = served
            .ops
            .show(ShowReq::new(Rev::from(landed["landed"].as_str().unwrap())));
        let key = shown.unwrap().land_key;
        let head = changeset_id(&b, "head").unwrap();
        assert_eq!(key, Some(sync_key("main", head)));
    }
}
