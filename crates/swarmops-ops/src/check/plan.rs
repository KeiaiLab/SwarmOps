//! `check plan` (M30-4): for one stage, which checks a change needs, each `run` or
//! `not_applicable` with one reason.
//!
//! ```text
//!   the candidate's checks ─▶ Matcher: each check's inputs
//!   the changed paths ─ read one by one (changed::Changed) ─▶ Matcher::see: the first match
//!                                                              of each check, no path kept
//!                                   ▼
//!   decide: per check of the stage, the first rule that holds
//!     the whole plan is widened (paths unknown, cut, unreadable)   run             widened
//!     it has inputs, and a changed path matches one                run             input_changed
//!     it has no inputs, and at least one path changed              run             no_inputs
//!     otherwise                                                    not_applicable  inputs_unchanged
//! ```
//!
//! The subjects: a changeset against its target ([`plan`]: what it changed since their merge
//! base), and the changed paths the caller gives with the check file of a directory
//! ([`plan_dir`], vouched). Planning reads only: no lock is taken and nothing is written. A plan
//! informs: `check run` runs every check of its stage, whatever a plan says.

use std::collections::BTreeSet;

use camino::Utf8Path;
use serde_json::json;
use swarmops_api::{
    CheckPlanOut, CheckPlanReason, CheckPlanReq, CheckPlanResp, CheckPlanSubject, CheckPlanVerdict,
    CheckStage, ErrorCode, MergeBaseReq, OpError, ResolveRevReq, Rev, ShowReq, StoreOps,
};

use super::changed::{Changed, Sep};
use super::inputs::Inputs;
use super::{Check, checks_at, checks_in};
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

/// The plan of `stage`: one entry for each check of `candidate` that is of it, sorted by name,
/// each by the first rule that holds for what `matcher` saw of the paths `changed` read.
///
/// `not_applicable` has one way in, the last rule: every changed path was read (no cause
/// widens the plan), and none is among the check's inputs.
pub fn decide(
    stage: CheckStage,
    candidate: &[Check],
    matcher: &Matcher,
    changed: &Changed,
) -> Vec<CheckPlanOut> {
    let mut checks: Vec<CheckPlanOut> = (candidate.iter())
        .filter(|check| check.is_of(stage))
        .map(|check| by_paths(check, matcher, changed))
        .collect();
    checks.sort_by(|a, b| a.name.cmp(&b.name));
    checks
}

/// `check` by what changed: widened with the plan, else by its inputs.
fn by_paths(check: &Check, matcher: &Matcher, changed: &Changed) -> CheckPlanOut {
    let entry = |verdict, reason| {
        CheckPlanOut::new(
            check.name.clone(),
            verdict,
            reason,
            check.definition_digest(),
        )
    };

    // 1. The whole plan is widened: every check runs, also one a path already matched.
    if let Some(cause) = changed.cause() {
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
    if check.inputs.is_empty() && changed.count() > 0 {
        return entry(CheckPlanVerdict::Run, CheckPlanReason::NoInputs);
    }
    entry(
        CheckPlanVerdict::NotApplicable,
        CheckPlanReason::InputsUnchanged,
    )
}

/// `check_plan`: the plan of `req.stage` for the changeset `req.rev` against `req.target`: what
/// it changed since their merge base, matched against the inputs of the checks its own tree
/// describes. Both revisions are resolved once. Reads the store; writes nothing.
pub fn plan(store: &dyn StoreOps, req: CheckPlanReq) -> Result<CheckPlanResp, OpError> {
    let CheckPlanReq {
        rev,
        target,
        landed,
        base,
        stage,
        checks,
        ..
    } = req;
    if landed || base.is_some() {
        return Err(invalid(
            "a plan of a change as it would land is not built yet".to_owned(),
            "landed",
        ));
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

    // 3. The plan, and of it the checks asked for.
    let mut resp = CheckPlanResp::new(false, CheckPlanSubject::Changeset, stage);
    resp.checks = decide(stage, &candidate, &matcher, &changed);
    resp.rev = Some(id.clone());
    resp.tree = Some(tree);
    resp.target = Some(target);
    resp.onto = Some(onto);
    resp.base = base;
    resp.file = file.map(str::to_owned);
    resp.version = version;
    resp.changed = changed.count();
    resp.widened = changed.cause();
    only(resp, &checks, &candidate, &id)
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
pub fn plan_dir(
    dir: &Utf8Path,
    stage: CheckStage,
    checks: &[String],
    given: Given<'_>,
) -> Result<CheckPlanResp, OpError> {
    // 1. The directory's checks.
    let (file, version, candidate) = checks_in(dir)?;

    // 2. The paths given, the flags' first; none given at all is not a change of no path.
    let mut matcher = Matcher::new(stage, &candidate);
    let changed = match given {
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

    // 3. The plan, and of it the checks asked for.
    let mut resp = CheckPlanResp::new(true, CheckPlanSubject::Paths, stage);
    resp.checks = decide(stage, &candidate, &matcher, &changed);
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
