//! `check plan` (M30-4, M30-5): for one stage, which checks a change needs, each `run` or
//! `not_applicable` with one reason.
//!
//! ```text
//!   the candidate's checks ─▶ Matcher: each check's inputs
//!   the changed paths ─ read one by one (changed::Changed) ─▶ Matcher::see: the first match
//!                                                              of each check, no path kept
//!   the checks at the target's head (Target) ──┐
//!                                              ▼
//!   decide: per name of either side, the first rule that holds
//!     its definition is not the target's head's     run             definition_changed
//!     the whole plan is widened                     run             widened
//!     a changed path is among its inputs            run             input_changed
//!     it has no inputs, and a path changed          run             no_inputs
//!     otherwise                                     not_applicable  inputs_unchanged
//! ```
//!
//! The subjects: a changeset against its target ([`plan`]: what it changed since their merge
//! base); a change as `land_check` lands it (`landed`: the item's paths and tree); a workspace
//! ([`plan_ws`]: its committed chain and its edits); and the changed paths the caller gives
//! with the check file of a directory ([`plan_dir`], vouched).
//! Planning reads only: no lock is taken and nothing is written. A plan informs: `check run`
//! runs every check of its stage, whatever a plan says.
//!
//! A plan fails closed (M30-5). Changed paths that are unknown, cut or unreadable, a target
//! whose check file does not read, and a check file changed where it cannot be compared widen
//! the whole plan; a definition added, renamed, deleted or not the target's head's runs. An
//! error is an error, never a plan with fewer checks: [`decide`] gives one entry for each name
//! of either side.

use std::collections::BTreeSet;

use camino::Utf8Path;
use serde_json::json;
use swarmops_api::{
    CheckDefinitionChange, CheckPlanOut, CheckPlanReason, CheckPlanReq, CheckPlanResp,
    CheckPlanSubject, CheckPlanVerdict, CheckPlanWidened, CheckStage, ErrorCode, ItemOutcome,
    LandCheckItemOut, LandCheckReq, LandCheckResp, LandItemIn, MergeBaseReq, OpError, PathChange,
    ResolveRevReq, Rev, ShowReq, StoreOps,
};

use swarmops_ws::Locate;

use super::changed::{CHECK_FILE_PATHS, Changed, Sep};
use super::inputs::Inputs;
use super::{Check, MAX_PLAN_PATHS, checks_at, checks_in};
use crate::error;

/// The candidate's checks of one stage, each with its inputs and the first changed path seen
/// that they match: what the changed paths are matched against as they are read.
#[derive(Debug, Clone)]
pub struct Matcher {
    /// One slot a check, in the candidate's order.
    slots: Vec<Slot>,
}

/// One check of a [`Matcher`].
#[derive(Debug, Clone)]
struct Slot {
    /// The check's name.
    name: String,
    /// Its inputs, read.
    inputs: Inputs,
    /// The first changed path seen that its inputs match, and the index of the first pattern,
    /// in declared order, that matches it. None for the whole tree, which has no pattern.
    first: Option<(String, usize)>,
}

impl Matcher {
    /// The checks of `candidate` that are of `stage`, nothing seen yet. A check whose patterns
    /// are none the check file's reader takes is given the whole tree.
    pub fn new(stage: CheckStage, candidate: &[Check]) -> Matcher {
        let slots = (candidate.iter())
            .filter(|check| check.is_of(stage))
            .map(|check| Slot {
                name: check.name.clone(),
                inputs: Inputs::parse(&check.inputs).unwrap_or_default(),
                first: None,
            })
            .collect();
        Matcher { slots }
    }

    /// Sees one changed path: each check that has no match yet takes it as its first, if its
    /// inputs match it. The path is not kept otherwise.
    pub fn see(&mut self, path: &str) {
        for slot in &mut self.slots {
            if slot.first.is_some() {
                continue;
            }
            if let Some(pattern) = slot.inputs.first_match(path) {
                slot.first = Some((path.to_owned(), pattern));
            }
        }
    }

    /// The first changed path seen that the inputs of check `name` match, with the index of
    /// the pattern that matches it.
    fn first(&self, name: &str) -> Option<&(String, usize)> {
        let slot = self.slots.iter().find(|slot| slot.name == name)?;
        slot.first.as_ref()
    }
}

/// What a plan compares the candidate's definitions with.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum Target<'t> {
    /// The checks of the check file at the target's head.
    Checks(&'t [Check]),
    /// The target's check file does not read: no comparison can be made, and the whole plan
    /// is widened, `target_unread`.
    Unread,
    /// No target is at hand: the directory form, whose caller vouches that the definitions
    /// are the target's.
    Unknown,
}

/// Why the whole plan is widened, if it is: the first cause that holds, in the causes' own
/// order, among what reading the changed paths came to and a target that does not read.
pub fn cause_of(target: &Target<'_>, changed: &Changed) -> Option<CheckPlanWidened> {
    let unread = matches!(target, Target::Unread).then_some(CheckPlanWidened::TargetUnread);
    match (changed.cause(), unread) {
        (Some(paths), Some(unread)) => Some(paths.min(unread)),
        (paths, unread) => paths.or(unread),
    }
}

/// The plan of `stage`: exactly one entry for each name that `candidate` or `target` has in
/// it, sorted by name, each by the first rule that holds:
///
/// ```text
///   1  its definition is not the target's head's    run             definition_changed
///        in both, digests differ                      modified
///        the candidate's alone, its digest that of    renamed, was
///          exactly one name the target alone has
///        the candidate's alone, otherwise             added
///        the target's alone                           deleted (the target's digest)
///   2  the whole plan is widened                    run             widened, the cause
///   3  a changed path is among its inputs           run             input_changed
///   4  it has no inputs, and a path changed         run             no_inputs
///   5  otherwise                                    not_applicable  inputs_unchanged
/// ```
///
/// `not_applicable` has one way in, rule 5: every changed path was read, none is among the
/// check's inputs, and its definition is the target's head's. `stages` is not in a digest:
/// membership of the planned stage is what is compared, so a check moved out of the stage is
/// `deleted` for it and one moved in `added`.
pub fn decide(
    stage: CheckStage,
    candidate: &[Check],
    target: Target<'_>,
    matcher: &Matcher,
    changed: &Changed,
) -> Vec<CheckPlanOut> {
    // 1. Each side's checks of the stage, with their digests; the plan's cause.
    let of_stage = |checks: &[Check]| -> Vec<(String, String)> {
        (checks.iter())
            .filter(|check| check.is_of(stage))
            .map(|check| (check.name.clone(), check.definition_digest()))
            .collect()
    };
    let ours = of_stage(candidate);
    let theirs = match target {
        Target::Checks(checks) => Some(of_stage(checks)),
        Target::Unread | Target::Unknown => None,
    };
    let cause = cause_of(&target, changed);

    // 2. The candidate's checks: a definition that differs, else by the paths.
    let mut plan = Vec::with_capacity(ours.len());
    for check in candidate.iter().filter(|check| check.is_of(stage)) {
        let differs = (theirs.as_deref()).and_then(|theirs| differs(&check.name, &ours, theirs));
        plan.push(match differs {
            Some((definition, was)) => {
                let mut out = CheckPlanOut::new(
                    check.name.clone(),
                    CheckPlanVerdict::Run,
                    CheckPlanReason::DefinitionChanged,
                    check.definition_digest(),
                );
                out.definition = Some(definition);
                out.was = was;
                out
            }
            None => by_paths(check, matcher, changed.count(), cause),
        });
    }

    // 3. The checks only the target's head has: deleted, listed with the target's digest.
    for (name, digest) in theirs.iter().flatten() {
        if ours.iter().any(|(ours, _)| ours == name) {
            continue;
        }
        let mut out = CheckPlanOut::new(
            name.clone(),
            CheckPlanVerdict::Run,
            CheckPlanReason::DefinitionChanged,
            digest.clone(),
        );
        out.definition = Some(CheckDefinitionChange::Deleted);
        plan.push(out);
    }
    plan.sort_by(|a, b| a.name.cmp(&b.name));
    plan
}

/// How the definition of the candidate's check `name` differs from the target's head's, and
/// for a rename the name it had there; none when the target has it under that name with the
/// same digest. `ours` and `theirs` are each side's names and digests.
fn differs(
    name: &str,
    ours: &[(String, String)],
    theirs: &[(String, String)],
) -> Option<(CheckDefinitionChange, Option<String>)> {
    let digest_of = |side: &[(String, String)], name: &str| {
        let found = side.iter().find(|(held, _)| held == name);
        found.map(|(_, digest)| digest.clone())
    };
    let digest = digest_of(ours, name)?;

    // 1. Under the same name: equal, or modified.
    if let Some(held) = digest_of(theirs, name) {
        return (held != digest).then_some((CheckDefinitionChange::Modified, None));
    }

    // 2. Under no name of the target's: renamed when exactly one name that only the target has
    //    carries this digest and no other name that only the candidate has does; else added.
    let alone = |side: &[(String, String)], other: &[(String, String)]| -> Vec<String> {
        (side.iter())
            .filter(|(_, held)| *held == digest)
            .filter(|(name, _)| digest_of(other, name).is_none())
            .map(|(name, _)| name.clone())
            .collect()
    };
    let (new, old) = (alone(ours, theirs), alone(theirs, ours));
    match (new.as_slice(), old.as_slice()) {
        ([_], [was]) => Some((CheckDefinitionChange::Renamed, Some(was.clone()))),
        _ => Some((CheckDefinitionChange::Added, None)),
    }
}

/// `check` by what changed: widened with the plan for `cause`, else by its inputs and the
/// `count` of paths read.
fn by_paths(
    check: &Check,
    matcher: &Matcher,
    count: u64,
    cause: Option<CheckPlanWidened>,
) -> CheckPlanOut {
    let entry = |verdict, reason| {
        CheckPlanOut::new(
            check.name.clone(),
            verdict,
            reason,
            check.definition_digest(),
        )
    };

    // 1. The whole plan is widened: every check runs, also one a path already matched.
    if let Some(cause) = cause {
        let mut out = entry(CheckPlanVerdict::Run, CheckPlanReason::Widened);
        out.widened = Some(cause);
        return out;
    }

    // 2. A changed path among its inputs: the first one read, and its first pattern.
    if let Some((path, pattern)) = matcher.first(&check.name) {
        let mut out = entry(CheckPlanVerdict::Run, CheckPlanReason::InputChanged);
        out.path = Some(path.clone());
        out.pattern = check.inputs.get(*pattern).cloned();
        return out;
    }

    // 3. No inputs: the whole tree, so any changed path is one of its own.
    if check.inputs.is_empty() && count > 0 {
        return entry(CheckPlanVerdict::Run, CheckPlanReason::NoInputs);
    }
    entry(
        CheckPlanVerdict::NotApplicable,
        CheckPlanReason::InputsUnchanged,
    )
}

/// `check_plan`: the plan of `req.stage` for the changeset `req.rev` against `req.target`: what
/// it changed since their merge base, matched against the inputs of the checks its own tree
/// describes, each definition compared with the check file at the target's head. Both
/// revisions are resolved once. Reads the store; writes nothing.
///
/// The candidate's check file not reading is `E_CHECK_FILE`: its checks cannot be listed
/// without it. The target's not reading widens the plan.
///
/// With `req.landed`, the change is planned as `land_check` lands it from `req.base`: the
/// item's paths and tree, not the changeset's own.
pub fn plan(store: &dyn StoreOps, req: CheckPlanReq) -> Result<CheckPlanResp, OpError> {
    if req.landed {
        return plan_landed(store, req);
    }
    let CheckPlanReq {
        rev,
        target,
        base,
        stage,
        checks,
        ..
    } = req;
    if base.is_some() {
        let what = "base says where a land's change starts: it is taken with landed alone";
        return Err(invalid(what.to_owned(), "base")
            .with_hint("give --landed with --base, or leave --base out"));
    }

    // 1. Both sides, each resolved once; the candidate's checks and its tree.
    let id = store.resolve_rev(ResolveRevReq::new(rev))?.id;
    let onto = store
        .resolve_rev(ResolveRevReq::new(Rev::from(target.as_str())))?
        .id;
    let (file, version, candidate) = checks_at(store, &id)?;
    let mut show = ShowReq::new(id.as_str());
    show.max_changes = 1;
    let tree = store.show(show)?.root;

    // 2. What the candidate changed since the merge base, matched as it is read. Two
    //    revisions with no shared history have no base: the changed paths are not known.
    let mut matcher = Matcher::new(stage, &candidate);
    let mut first = MergeBaseReq::new(id.as_str(), onto.as_str());
    (first.limit, first.max_bytes) = (1, 1);
    let (base, changed) = match store.merge_base(first) {
        Ok(found) => {
            let mut see = |path: &str| matcher.see(path);
            let read = Changed::from_store(store, &found.base, &id, Some(found.diff), &mut see)?;
            (Some(found.base), read)
        }
        Err(e) if no_shared_history(&e) => (None, Changed::unknown()),
        Err(e) => return Err(e),
    };

    // 3. The definitions at the target's head, to compare with; a file there that does not
    //    read leaves nothing to compare, and widens the plan.
    let theirs = target_at(store, &onto)?;
    let against = theirs.as_deref().map_or(Target::Unread, Target::Checks);

    // 4. The plan, and of it the checks asked for.
    let mut resp = CheckPlanResp::new(false, CheckPlanSubject::Changeset, stage);
    resp.checks = decide(stage, &candidate, against, &matcher, &changed);
    resp.rev = Some(id.clone());
    resp.tree = Some(tree);
    resp.target = Some(target);
    resp.onto = Some(onto);
    resp.base = base;
    resp.file = file.map(str::to_owned);
    resp.version = version;
    resp.changed = changed.count();
    resp.widened = cause_of(&against, &changed);
    only(resp, &checks, &candidate, &id)
}

/// The plan of `stage` for `rev` as `land_check` lands it on `target` from `base`: the item's
/// paths are the changed paths and its root the tree. A change with nothing to land, or one
/// already landed, changes no path. One that conflicts has no landed tree: `E_CONFLICT`, with
/// the item's report, and no plan.
///
/// No changeset names the landed tree, so its check file is read where the item's paths say
/// it comes from:
///
/// ```text
///   neither check file among the paths    the target's head's files     read at onto
///   one added, modified or deleted        the change's                  read at rev
///   one merged, or the paths cut          in no changeset: read at rev, and the plan widened
/// ```
fn plan_landed(store: &dyn StoreOps, req: CheckPlanReq) -> Result<CheckPlanResp, OpError> {
    let CheckPlanReq {
        rev,
        target,
        base,
        stage,
        checks,
        ..
    } = req;

    // 1. What landing the one change would come to.
    let mut change = LandItemIn::new(rev);
    change.base = base;
    let mut check = LandCheckReq::new(vec![change]);
    check.target.clone_from(&target);
    let LandCheckResp { onto, items, .. } = store.land_check(check)?;
    let Some(item) = items.into_iter().next() else {
        let what = "land_check answered no item for the one change";
        return Err(error::error(ErrorCode::Internal, what));
    };
    if item.outcome == ItemOutcome::Conflict {
        return Err(conflicts(&item, &target, &onto));
    }

    // 2. The item's paths, in path order, and where they say the landed tree's check file is.
    let mut changed = Changed::none();
    let mut file_at = onto.as_str();
    for path in (item.paths.iter()).filter(|path| CHECK_FILE_PATHS.contains(&path.path.as_str())) {
        file_at = item.head.as_str();
        if path.change == PathChange::Merged {
            changed.widen(CheckPlanWidened::CheckFileChanged);
        }
    }
    if item.paths_truncated {
        file_at = item.head.as_str();
        changed.widen(CheckPlanWidened::PathsTruncated);
    }
    let (file, version, candidate) = checks_at(store, file_at)?;
    let mut matcher = Matcher::new(stage, &candidate);
    for path in &item.paths {
        changed.path(&path.path, &mut |path| matcher.see(path));
    }

    // 3. The definitions at the target's head, and the plan.
    let theirs = target_at(store, &onto)?;
    let against = theirs.as_deref().map_or(Target::Unread, Target::Checks);
    let mut resp = CheckPlanResp::new(false, CheckPlanSubject::Landed, stage);
    resp.checks = decide(stage, &candidate, against, &matcher, &changed);
    resp.rev = Some(item.head.clone());
    resp.tree = item.root.filter(|_| item.outcome == ItemOutcome::Clean);
    resp.target = Some(target);
    resp.onto = Some(onto);
    resp.outcome = Some(item.outcome);
    resp.file = file.map(str::to_owned);
    resp.version = version;
    resp.changed = changed.count();
    resp.widened = cause_of(&against, &changed);
    only(resp, &checks, &candidate, &item.head)
}

/// `E_CONFLICT` for the change `item`, which conflicts with `target` at its head `onto`: the
/// item's report in `details.report`, as a land gives it, and `details.onto`.
fn conflicts(item: &LandCheckItemOut, target: &str, onto: &str) -> OpError {
    let report = serde_json::to_value(&item.report).unwrap_or_default();
    let what = format!(
        "{} conflicts with {target}: there is no landed tree to plan",
        item.head
    );
    let mut error = error::error(ErrorCode::Conflict, what).with_hint(format!(
        "swarmops land-check {} --target {target} shows the conflict; resolve it, then plan again",
        item.head
    ));
    error.details.insert("report".to_owned(), report);
    error.details.insert("onto".to_owned(), onto.into());
    error
}

/// `check plan --ws`: the plan of `stage` for the workspace `at` names: the paths of its
/// committed chain (the diff from its base to its head), then those of its edits against its
/// head. Read without the workspace's lock, as `ws status` reads it; refused as `check run`
/// refuses a workspace that is rebasing, stale, or whose files stand for another changeset
/// than its head. Edits are what this form is for: they are not refused.
///
/// ```text
///   the candidate's checks   the store's, at the head; the workspace's own files (no link
///                            followed) when an edit names the check file
///   the target's             at the head of the workspace's target
///   entries off the prefixes of a sparse workspace: edits in no list ─▶ paths_unknown
/// ```
///
/// A path both committed and edited is read twice: `changed` counts what was read. With an
/// edit no tree in the store is what was planned, and `tree` is omitted. The store is only
/// read; the workspace's scan index is kept, as `ws status` keeps it.
pub fn plan_ws(
    ctx: &swarmops_ws::Ctx<'_>,
    at: &Locate,
    stage: CheckStage,
    checks: &[String],
) -> Result<CheckPlanResp, OpError> {
    let limit = usize::try_from(MAX_PLAN_PATHS).unwrap_or(usize::MAX);
    plan_ws_within(ctx, at, stage, checks, limit)
}

/// [`plan_ws`], reading at most `limit` of the workspace's edits: more of them widen the
/// plan, `paths_truncated`.
fn plan_ws_within(
    ctx: &swarmops_ws::Ctx<'_>,
    at: &Locate,
    stage: CheckStage,
    checks: &[String],
    limit: usize,
) -> Result<CheckPlanResp, OpError> {
    // 1. The workspace's chain and its edits, as far as a plan reads them.
    let edits = swarmops_ws::check::edits(ctx, at, limit)?;
    let store = ctx.store;

    // 2. The candidate's checks: the head's, unless an edit changed the check file, which is
    //    then the workspace's own, read as a directory's and not vouched for by a caller.
    let edited = (edits.paths.iter()).any(|path| CHECK_FILE_PATHS.contains(&path.as_str()));
    let (file, version, candidate) = if edited {
        checks_in(&edits.dir).map_err(|mut e| {
            if let Some(vouched) = e.details.get_mut("vouched") {
                *vouched = false.into();
            }
            e
        })?
    } else {
        checks_at(store, &edits.head)?
    };

    // 3. The committed chain's paths, then the edits'; what cannot be listed widens the plan.
    let mut matcher = Matcher::new(stage, &candidate);
    let mut see = |path: &str| matcher.see(path);
    let mut changed = Changed::from_store(store, &edits.base, &edits.head, None, &mut see)?;
    let within = (edits.paths.iter()).all(|path| changed.path(path, &mut see));
    if edits.truncated && within {
        changed.widen(CheckPlanWidened::PathsTruncated);
    }
    if edits.outside {
        changed.widen(CheckPlanWidened::PathsUnknown);
    }

    // 4. The definitions at the head of the workspace's target, and the plan.
    let onto = store
        .resolve_rev(ResolveRevReq::new(Rev::from(edits.target.as_str())))?
        .id;
    let theirs = target_at(store, &onto)?;
    let against = theirs.as_deref().map_or(Target::Unread, Target::Checks);
    let mut resp = CheckPlanResp::new(false, CheckPlanSubject::Workspace, stage);
    resp.checks = decide(stage, &candidate, against, &matcher, &changed);
    if edits.paths.is_empty() {
        let mut show = ShowReq::new(edits.head.as_str());
        show.max_changes = 1;
        resp.tree = Some(store.show(show)?.root);
    }
    resp.edits = Some(u64::try_from(edits.paths.len()).unwrap_or(u64::MAX));
    resp.workspace = Some(edits.name);
    resp.target = Some(edits.target);
    resp.onto = Some(onto);
    resp.base = Some(edits.base);
    resp.file = file.map(str::to_owned);
    resp.version = version;
    resp.changed = changed.count();
    resp.widened = cause_of(&against, &changed);
    let list_at = edits.head.clone();
    resp.rev = Some(edits.head);
    only(resp, checks, &candidate, &list_at)
}

/// The changed paths the caller of the directory form gives.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum Given<'g> {
    /// None: neither a path nor a list. The changed paths are not known.
    None,
    /// The paths of `--changed`, as given, then those of a list, if one was given: its bytes
    /// and what ends its entries. A list of no entry is a change of no path.
    Paths {
        /// The paths given one by one, read first.
        flags: &'g [String],
        /// The list's bytes and its separator.
        list: Option<(&'g [u8], Sep)>,
    },
}

/// `check plan --dir`: the plan of `stage` for the changed paths `given`, against the checks
/// the files below `dir` describe, as `check list --dir` reads them. The answer is `vouched`:
/// only the caller stands behind the file and the paths, and no gate accepts it. Opens no
/// store, and looks up no path below `dir`: a deleted file is a changed path.
///
/// No target is at hand, so no check is `definition_changed` here: the caller vouches that
/// the definitions are the target's, and a check file among the changed paths widens the
/// whole plan, `check_file_changed`.
pub fn plan_dir(
    dir: &Utf8Path,
    stage: CheckStage,
    checks: &[String],
    given: Given<'_>,
) -> Result<CheckPlanResp, OpError> {
    // 1. The directory's checks.
    let (file, version, candidate) = checks_in(dir)?;

    // 2. The paths given, the flags' first; none given at all is not a change of no path. No
    //    target is at hand to compare a changed check file with: it widens the plan.
    let mut matcher = Matcher::new(stage, &candidate);
    let mut changed = match given {
        Given::None => Changed::unknown(),
        Given::Paths { flags, list } => {
            let mut see = |path: &str| matcher.see(path);
            let mut changed = Changed::none();
            let within = flags.iter().all(|path| changed.path(path, &mut see));
            if let Some((bytes, sep)) = list.filter(|_| within) {
                changed.list(bytes, sep, &mut see);
            }
            changed
        }
    };
    if changed.check_file() {
        changed.widen(CheckPlanWidened::CheckFileChanged);
    }

    // 3. The plan, and of it the checks asked for.
    let mut resp = CheckPlanResp::new(true, CheckPlanSubject::Paths, stage);
    resp.checks = decide(stage, &candidate, Target::Unknown, &matcher, &changed);
    resp.file = file.map(str::to_owned);
    resp.version = version;
    resp.changed = changed.count();
    resp.widened = changed.cause();
    only(resp, checks, &candidate, &format!("--dir {dir}"))
}

/// `resp` with only the checks `names` names, each once; every check when there is none.
/// `E_INVALID_ARG` for a name the plan does not hold: `details.reason` `stage`, with the
/// check's stages, when the candidate's file `all` defines it for another stage, else `check`.
/// `list_at` is what `check list` takes to list the same file.
pub fn only(
    mut resp: CheckPlanResp,
    names: &[String],
    all: &[Check],
    list_at: &str,
) -> Result<CheckPlanResp, OpError> {
    let stage = resp.stage;
    let wanted: BTreeSet<&str> = names.iter().map(String::as_str).collect();
    for name in &wanted {
        if resp.checks.iter().any(|check| check.name == *name) {
            continue;
        }
        let Some(check) = all.iter().find(|check| check.name == *name) else {
            let what = format!("no check of the plan is named {name:?}");
            let mut error = invalid(what, "check");
            error.details.insert("check".to_owned(), (*name).into());
            return Err(error.with_hint(format!("swarmops check list {list_at} --stage {stage}")));
        };
        let what = format!("the check {name} is not of the stage {stage}");
        let mut error = invalid(what, "stage");
        error.details.insert("check".to_owned(), (*name).into());
        error
            .details
            .insert("stages".to_owned(), json!(check.stages));
        return Err(error.with_hint(format!(
            "give the check's stage: --stage {}",
            check.stages[0]
        )));
    }
    if !wanted.is_empty() {
        resp.checks
            .retain(|check| wanted.contains(check.name.as_str()));
    }
    Ok(resp)
}

/// The checks of the check file at the target's head `onto`, read as `check list` reads it;
/// none when that file does not read (`E_CHECK_FILE`). Any other error is the error.
fn target_at(store: &dyn StoreOps, onto: &str) -> Result<Option<Vec<Check>>, OpError> {
    match checks_at(store, onto) {
        Ok((_, _, checks)) => Ok(Some(checks)),
        Err(e) if e.code == ErrorCode::CheckFile.as_str() => Ok(None),
        Err(e) => Err(e),
    }
}

/// Whether `e` is `merge_base`'s answer for two revisions whose first-parent chains share no
/// changeset: `E_INVALID_ARG` naming the two heads and nothing else. Every other error of it
/// (a store without the operation among them) is an error.
fn no_shared_history(e: &OpError) -> bool {
    e.code == ErrorCode::InvalidArg.as_str()
        && e.details.len() == 2
        && e.details.contains_key("head")
        && e.details.contains_key("theirs")
}

/// `E_INVALID_ARG` saying `what`, with `details.reason`.
fn invalid(what: String, reason: &str) -> OpError {
    let mut error = error::error(ErrorCode::InvalidArg, what);
    error.details.insert("reason".to_owned(), reason.into());
    error
}

#[cfg(test)]
mod tests;
