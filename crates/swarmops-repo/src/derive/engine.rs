//! The engine of the derived-data framework ([`super`]): one run per value asked for ([`run`]),
//! over an explicit stack of frames on the heap, so that no chain of values, however long, is a
//! chain of calls on the Rust stack.
//!
//! A frame works on one value asked for, a node. Its chain starts with that node and grows by one
//! node for each `canonical` redirect, every node of it having the same value; the last is the
//! node worked on. The work on it goes through three states ([`State`]): look for its value in
//! the run and in the store; else call `canonical`, which may redirect it; else derive its
//! dependencies, one frame at a time, then call `derive`. A call that returns
//! [`DeriveError::Missing`] gets a frame pushed for the value it waits for, and is made again once
//! that frame is done. A frame is done once the value of its last node is known, found or derived
//! (and stored): it stores the memos `MEMO_EVERY` asks for along its chain, and hands the value to
//! the frame below, which holds it in the run's [`Memo`] until it is done itself.
//!
//! Every node of every frame's chain is on the stack: a dependency, a redirect or a value waited
//! for that is on the stack already is a cycle, and the run's error.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use swarmops_core::{Changeset, ChangesetId, ObjectKind};
use swarmops_store::{DerivedSlot, DerivedStore, ObjectStore, StoreError};

use super::derivation;
use super::{DeriveCtx, DeriveError, Need, Registry, Subject};
use crate::error::RepoError;
use crate::repo;

/// A value's postcard bytes, shared between the memo, the frames and the caller.
pub(super) type Value = Arc<[u8]>;

/// One value the engine works on: derivation `deriv`'s value of changeset `cs` under a key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct Node {
    /// The derivation's place in the registry ([`Registry::entry`]).
    pub(super) deriv: usize,
    /// The changeset the value is about.
    pub(super) cs: ChangesetId,
    /// The key's postcard bytes.
    pub(super) key: Arc<[u8]>,
}

/// The values a run holds besides the derived store's: the ones a frame on the stack waits on (its
/// last node's dependencies, and the values its calls asked for), until that frame is done; and
/// the ones the run derived and could not store, until the run ends, since the store cannot give
/// them back. So a value a call reads is read from the store at most once per frame, and a store
/// that takes no value costs nothing but the puts.
#[derive(Default)]
pub(super) struct Memo {
    values: HashMap<Node, Held>,
}

/// A value the memo holds, and why.
struct Held {
    value: Value,
    /// How many frames on the stack wait on it: it is dropped once none does, unless it is kept.
    pins: usize,
    /// Its put failed: the run keeps it to its end.
    kept: bool,
}

impl Memo {
    /// `node`'s value, if the run holds it.
    pub(super) fn get(&self, node: &Node) -> Option<&[u8]> {
        self.values.get(node).map(|held| &*held.value)
    }

    /// `node`'s value, shared, if the run holds it.
    fn value(&self, node: &Node) -> Option<Value> {
        self.values.get(node).map(|held| Arc::clone(&held.value))
    }

    /// Holds `value`, `node`'s, for one more frame.
    fn pin(&mut self, node: Node, value: Value) {
        let held = self.values.entry(node).or_insert_with(|| Held {
            value,
            pins: 0,
            kept: false,
        });
        held.pins += 1;
    }

    /// One frame fewer waits on `node`'s value: it is dropped once none does, unless it is kept.
    fn unpin(&mut self, node: &Node) {
        if let Some(held) = self.values.get_mut(node) {
            held.pins = held.pins.saturating_sub(1);
            if held.pins == 0 && !held.kept {
                self.values.remove(node);
            }
        }
    }

    /// Keeps `value`, `node`'s, to the end of the run.
    fn keep(&mut self, node: Node, value: Value) {
        let held = self.values.entry(node).or_insert_with(|| Held {
            value,
            pins: 0,
            kept: false,
        });
        held.kept = true;
    }
}

/// One value asked for, and the nodes `canonical` redirected it through.
struct Frame {
    /// The node asked for, then each node a redirect led to, all of one value: the last is the
    /// node worked on.
    chain: Vec<Node>,
    /// Where the work on the last node stands.
    state: State,
    /// The nodes whose values the memo holds for this frame: the last node's dependencies, and
    /// the values its calls waited on. Released when the frame is done.
    pins: Vec<Node>,
}

/// Where the work on a frame's last node stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Nothing is known of it yet: its value may be in the run or in the store.
    Look,
    /// Its value is in neither: `canonical` says whether it redirects (again, after a value the
    /// call waited on).
    Canonical,
    /// It redirects nowhere: its dependencies first, then `derive` (again, after a value the call
    /// waited on).
    Derive,
}

/// A run's state: what it reads and writes through, the memo, and the stack.
struct Engine<'r> {
    objects: &'r dyn ObjectStore,
    store: &'r DerivedStore,
    registry: &'r Registry,
    memo: Memo,
    /// The frames, the value asked for at the bottom; the top one is worked on.
    stack: Vec<Frame>,
    /// Every node of every frame's chain: a node met again while it is here is a cycle.
    on_stack: HashSet<Node>,
}

/// `root`'s value: read back if the store holds it; otherwise derived, with every value it needs
/// that the store lacks, and stored (see [the module documentation](self)). Objects are read
/// through `objects`, values through `store`; `registry` holds the derivations. A failed read of
/// the store is the error, and so is a missing object; a failed put is dropped.
pub(super) fn run(
    objects: &dyn ObjectStore,
    store: &DerivedStore,
    registry: &Registry,
    root: Node,
) -> Result<Value, RepoError> {
    let mut engine = Engine {
        objects,
        store,
        registry,
        memo: Memo::default(),
        stack: Vec::new(),
        on_stack: HashSet::new(),
    };
    engine.push(root, State::Look);
    loop {
        if let Some(value) = engine.step()? {
            return Ok(value);
        }
    }
}

impl Engine<'_> {
    /// Works on the top frame until it pushes a frame, follows a redirect or is done: the value
    /// asked for once the bottom frame is done.
    fn step(&mut self) -> Result<Option<Value>, RepoError> {
        let registry = self.registry;
        let frame = self.stack.last().expect("a run ends with its bottom frame");
        let node = frame.chain.last().expect("a chain is never empty").clone();
        let state = frame.state;
        if state == State::Look
            && let Some(value) = self.look(&node)?
        {
            return Ok(self.done(value));
        }
        let changeset = self.changeset(node.cs)?;
        let subject = Subject {
            id: node.cs,
            changeset: &changeset,
        };
        let derivation = &*registry.entry(node.deriv).derivation;
        if state != State::Derive {
            match derivation.canonical(&self.ctx(), subject, &node.key) {
                Ok(Some((cs, key))) => {
                    let deriv = node.deriv;
                    let key = key.into();
                    self.follow(Node { deriv, cs, key })?;
                    return Ok(None);
                }
                Ok(None) => self.top().state = State::Derive,
                Err(DeriveError::Missing(need)) => {
                    self.top().state = State::Canonical;
                    self.wait_for(need)?;
                    return Ok(None);
                }
                Err(DeriveError::Repo(e)) => return Err(e),
            }
        }
        for (cs, key) in derivation.deps(subject, &node.key)? {
            let dep = Node {
                deriv: node.deriv,
                cs,
                key: key.into(),
            };
            if self.on_stack.contains(&dep) {
                return Err(self.cycle(&dep));
            }
            if self.memo.get(&dep).is_some() {
                continue;
            }
            if let Some(value) = self.read(&dep)? {
                self.pin(dep, value);
            } else {
                // Its value was looked for just now: none is in the run or the store.
                self.push(dep, State::Canonical);
                return Ok(None);
            }
        }
        match derivation.derive(&self.ctx(), subject, &node.key) {
            Ok(value) => {
                let value = Value::from(value);
                // A put that fails is dropped, the value kept for the rest of the run.
                if self.write(&node, &value).is_err() {
                    self.memo.keep(node, Arc::clone(&value));
                }
                Ok(self.done(value))
            }
            Err(DeriveError::Missing(need)) => {
                self.wait_for(need)?;
                Ok(None)
            }
            Err(DeriveError::Repo(e)) => Err(e),
        }
    }

    /// Ends the top frame with `value`, the value of every node of its chain: stores it at the
    /// memos `MEMO_EVERY = Some(n)` asks for (for a chain of more than `n` redirects, its first
    /// node and every `n`-th from there), releases the values the frame held, and hands the value
    /// to the frame below, which holds it until it is done; or returns it, from the bottom frame.
    fn done(&mut self, value: Value) -> Option<Value> {
        let frame = self.stack.pop().expect("a frame to end");
        let mut chain = frame.chain;
        if let Some(every) = self.registry.entry(chain[0].deriv).memo_every {
            let steps = chain.len() - 1;
            let every = usize::try_from(every).expect("a u32 fits in a usize");
            if steps > every {
                for memo in chain[..steps].iter().step_by(every) {
                    // A memo that is not stored costs a later walk its steps, nothing else.
                    let _ = self.write(memo, &value);
                }
            }
        }
        for node in &chain {
            self.on_stack.remove(node);
        }
        for node in &frame.pins {
            self.memo.unpin(node);
        }
        chain.truncate(1);
        let origin = chain.pop().expect("a chain is never empty");
        match self.stack.last_mut() {
            Some(below) => {
                below.pins.push(origin.clone());
                self.memo.pin(origin, value);
                None
            }
            None => Some(value),
        }
    }

    /// Adds `next`, where a `canonical` redirect of the top frame's last node leads, to that
    /// frame's chain, to be looked for: a cycle if it is on the stack.
    fn follow(&mut self, next: Node) -> Result<(), RepoError> {
        if self.on_stack.contains(&next) {
            return Err(self.cycle(&next));
        }
        self.on_stack.insert(next.clone());
        let top = self.top();
        top.chain.push(next);
        top.state = State::Look;
        Ok(())
    }

    /// What the top frame's call waits for, as it returned [`DeriveError::Missing`]: a frame of
    /// its own, unless it is on the stack (a cycle) or the store has it after all (stored since
    /// the call looked, by another process: the top frame holds it, and the call is made again).
    /// A value the run holds is the derivation's error: [`DeriveCtx::value`] gives what the run
    /// holds, so a `Missing` for it would come back on every call.
    fn wait_for(&mut self, need: Need) -> Result<(), RepoError> {
        let Need { name, cs, key } = need;
        let deriv = self.registry.position(name).ok_or_else(|| {
            derivation::derivation_error(name, "not registered: Repo::register adds a derivation")
        })?;
        let node = Node { deriv, cs, key };
        if self.on_stack.contains(&node) {
            return Err(self.cycle(&node));
        }
        if self.memo.get(&node).is_some() {
            let top = self.stack.last().expect("a frame waits");
            let asker = top.chain.last().expect("a chain is never empty");
            return Err(derivation::derivation_error(
                self.registry.entry(asker.deriv).name,
                format!(
                    "returned Missing for {name}'s value of changeset {cs}, which it was given"
                ),
            ));
        }
        match self.read(&node)? {
            Some(value) => self.pin(node, value),
            None => self.push(node, State::Canonical),
        }
        Ok(())
    }

    /// `node`'s value, if the run holds it or the store does.
    fn look(&self, node: &Node) -> Result<Option<Value>, RepoError> {
        match self.memo.value(node) {
            Some(value) => Ok(Some(value)),
            None => self.read(node),
        }
    }

    /// `node`'s value, if the store holds it. A failed read is the run's error: the engine does
    /// not derive over a store it cannot read.
    fn read(&self, node: &Node) -> Result<Option<Value>, RepoError> {
        let value = self.store.get(&self.slot(node))?;
        Ok(value.map(Value::from))
    }

    /// Stores `value` as `node`'s: `true` if this put placed it.
    fn write(&self, node: &Node, value: &[u8]) -> Result<bool, StoreError> {
        self.store.put(&self.slot(node), value)
    }

    /// The derived store's slot of `node`'s value: its derivation's name and version, its
    /// changeset, its key.
    fn slot<'n>(&self, node: &'n Node) -> DerivedSlot<'n> {
        let entry = self.registry.entry(node.deriv);
        DerivedSlot {
            name: entry.name,
            version: entry.version,
            subject: node.cs.0,
            key: &node.key,
        }
    }

    /// Changeset `cs`, decoded from its object: the subject of the node worked on.
    fn changeset(&self, cs: ChangesetId) -> Result<Changeset, RepoError> {
        let payload = repo::object(self.objects, cs.0, ObjectKind::Changeset)?;
        Ok(Changeset::decode(&payload)?)
    }

    /// What a call of `canonical` or `derive` reads through: the objects, the store, the
    /// registry, and the values the run holds.
    fn ctx(&self) -> DeriveCtx<'_> {
        DeriveCtx {
            objects: self.objects,
            store: self.store,
            registry: self.registry,
            memo: &self.memo,
        }
    }

    /// Pushes a frame for `node`, in `state`: [`State::Look`] for the value asked for,
    /// [`State::Canonical`] for one looked for already.
    fn push(&mut self, node: Node, state: State) {
        self.on_stack.insert(node.clone());
        self.stack.push(Frame {
            chain: vec![node],
            state,
            pins: Vec::new(),
        });
    }

    /// Holds `value`, `node`'s, for the top frame, until it is done.
    fn pin(&mut self, node: Node, value: Value) {
        self.top().pins.push(node.clone());
        self.memo.pin(node, value);
    }

    /// The frame worked on.
    fn top(&mut self) -> &mut Frame {
        self.stack.last_mut().expect("a frame is worked on")
    }

    /// The error of meeting `node` again while it is on the stack: its value would need itself.
    fn cycle(&self, node: &Node) -> RepoError {
        let name = self.registry.entry(node.deriv).name;
        derivation::derivation_error(
            name,
            format!(
                "a cycle: its value of changeset {} is needed while it is being derived",
                node.cs
            ),
        )
    }
}
