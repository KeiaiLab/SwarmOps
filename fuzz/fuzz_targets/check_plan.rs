//! A plan and the reader of its changed paths (M30-4, M30-5): `check plan`'s decision never
//! panics on any check files and any list of paths, and never plans a check as not needed
//! unless every path was read and none is among its inputs. The input:
//!
//! ```text
//! <byte: bit 0 the separator (0 a line feed, 1 NUL); bits 1 and 2 the target (0 or 3 none at
//!        hand, 1 the second file, 2 one that does not read); bits 3 to 5 the stage>
//! <the candidate's check file>\0\0\n<the target's check file>\0\0\n<the list of changed paths>
//! ```
//!
//! A candidate file that is refused gives no plan. Otherwise the plan holds one entry for each
//! name either side has in the stage, sorted, none twice. Every `not_applicable` entry stands
//! on a plan that is not widened, a list that held no unreadable entry and was not cut, a
//! definition that is the target's under the same name (or no target), and inputs that no entry
//! of the list matches, found again here by a plain loop of its own. Every `input_changed`
//! entry names an entry of the list and one of the check's patterns that matches it. The same
//! input twice gives the same plan.
#![no_main]

use libfuzzer_sys::fuzz_target;
use swarmops_api::{
    CheckDefinitionChange, CheckPlanOut, CheckPlanReason, CheckPlanVerdict, CheckPlanWidened,
    CheckStage,
};
use swarmops_core::RepoPath;
use swarmops_ops::check::changed::{Changed, Sep};
use swarmops_ops::check::inputs::{Inputs, resolve_paths};
use swarmops_ops::check::plan::{Matcher, Target, cause_of, decide};
use swarmops_ops::check::{self, Check, MAX_PLAN_PATH_BYTES, MAX_PLAN_PATHS};

/// What parts the input's three files.
const PART: &[u8] = b"\0\0\n";

fuzz_target!(|data: &[u8]| {
    // 1. The mode, and the three parts.
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    let sep = if mode & 1 == 0 { Sep::Line } else { Sep::Nul };
    let stage = CheckStage::ALL[usize::from((mode >> 3) & 7) % CheckStage::ALL.len()];
    let (candidate, rest) = parted(rest);
    let (theirs, list) = parted(rest);

    // 2. The candidate's checks, or no plan; the target's, by the mode.
    let Ok(candidate) = check::parse(candidate) else {
        return;
    };
    let candidate = candidate.checks;
    let read = check::parse(theirs).map(|file| file.checks);
    let target = match ((mode >> 1) & 3, &read) {
        (1, Ok(checks)) => Target::Checks(checks),
        (1, Err(_)) | (2, _) => Target::Unread,
        _ => Target::Unknown,
    };

    // 3. The plan, and the same again.
    let (plan, changed) = planned(stage, &candidate, target, list, sep);
    let (again, _) = planned(stage, &candidate, target, list, sep);
    assert_eq!(plan, again, "the same input, another plan");
    assert_eq!(format!("{plan:?}"), format!("{again:?}"));

    // 4. One entry for each name of either side in the stage, sorted, none twice.
    let of_stage = |checks: &[Check]| -> Vec<String> {
        let held = checks.iter().filter(|check| check.is_of(stage));
        held.map(|check| check.name.clone()).collect()
    };
    let mut names = of_stage(&candidate);
    if let Target::Checks(checks) = target {
        names.extend(of_stage(checks));
    }
    names.sort();
    names.dedup();
    let listed: Vec<String> = plan.iter().map(|entry| entry.name.clone()).collect();
    assert_eq!(listed, names);

    // 5. Each entry, against the list read again by this target's own loop.
    let entries = entries_of(list, sep);
    let cause = cause_of(&target, &changed);
    for entry in &plan {
        holds(entry, stage, &candidate, target, &entries, cause);
    }
});

/// `bytes` up to the first [`PART`], and what follows it; all of it and nothing without one.
fn parted(bytes: &[u8]) -> (&[u8], &[u8]) {
    match bytes.windows(PART.len()).position(|at| at == PART) {
        Some(at) => (&bytes[..at], &bytes[at + PART.len()..]),
        None => (bytes, &[]),
    }
}

/// The plan of `stage` for `candidate` against `target` when the paths of `list` changed, and
/// what reading the list came to.
fn planned(
    stage: CheckStage,
    candidate: &[Check],
    target: Target<'_>,
    list: &[u8],
    sep: Sep,
) -> (Vec<CheckPlanOut>, Changed) {
    let mut matcher = Matcher::new(stage, candidate);
    let changed = Changed::from_list(list, sep, &mut |path| matcher.see(path));
    (
        decide(stage, candidate, target, &matcher, &changed),
        changed,
    )
}

/// The entries of `list`, ended by `sep`, the empty ones passed over: each as a path, or none
/// for one that is no changed path. This target's own reading, apart from the reader's.
fn entries_of(list: &[u8], sep: Sep) -> Vec<Option<&str>> {
    let end = if sep == Sep::Line { b'\n' } else { 0 };
    let entries = list.split(|&b| b == end).filter(|bytes| !bytes.is_empty());
    entries.map(|bytes| path_of(bytes, sep)).collect()
}

/// `bytes` as a changed path: UTF-8, a repository path within its bound, and in a list of
/// lines neither quoted nor holding a control character.
fn path_of(bytes: &[u8], sep: Sep) -> Option<&str> {
    let path = std::str::from_utf8(bytes).ok()?;
    let quoted = path.starts_with('"') || path.chars().any(|c| c.is_ascii_control());
    let fits = path.len() <= MAX_PLAN_PATH_BYTES && RepoPath::new(path).is_ok();
    (fits && !(sep == Sep::Line && quoted)).then_some(path)
}

/// `entry` keeps the plan's rules for the check of `stage` it names.
fn holds(
    entry: &CheckPlanOut,
    stage: CheckStage,
    candidate: &[Check],
    target: Target<'_>,
    entries: &[Option<&str>],
    cause: Option<CheckPlanWidened>,
) {
    let named = |check: &&Check| check.name == entry.name && check.is_of(stage);
    let ours = candidate.iter().find(named);
    let readable = || entries.iter().flatten().copied();
    let whole = entries.iter().all(Option::is_some) && entries.len() as u64 <= MAX_PLAN_PATHS;
    match (entry.verdict, entry.reason) {
        // Not needed: nothing widened, every entry read, the target's definition, no match.
        (CheckPlanVerdict::NotApplicable, reason) => {
            assert_eq!(reason, CheckPlanReason::InputsUnchanged, "{entry:?}");
            assert_eq!(cause, None, "{entry:?}");
            assert!(whole, "{entry:?}: {entries:?}");
            let check = ours.expect("a check the candidate has");
            assert_eq!(entry.definition_digest, check.definition_digest());
            if let Target::Checks(theirs) = target {
                let held = theirs.iter().find(named);
                let held = held.expect("the target has it under the same name");
                assert_eq!(held.definition_digest(), entry.definition_digest);
            }
            let inputs = Inputs::parse(&check.inputs).expect("inputs the reader took");
            assert_eq!(resolve_paths(&inputs, readable()), Vec::<&str>::new());
            assert!(inputs.whole_tree() == check.inputs.is_empty(), "{entry:?}");
        }
        // A changed path among its inputs: an entry of the list, and a pattern that takes it.
        (CheckPlanVerdict::Run, CheckPlanReason::InputChanged) => {
            let check = ours.expect("a check the candidate has");
            let (path, pattern) = (entry.path.as_deref(), entry.pattern.clone());
            let (path, pattern) = (path.expect("a path"), pattern.expect("a pattern"));
            assert!(readable().any(|read| read == path), "{entry:?}");
            assert!(check.inputs.contains(&pattern), "{entry:?}");
            let one = Inputs::parse(std::slice::from_ref(&pattern)).expect("a pattern");
            assert!(one.matches(path), "{entry:?}");
            assert_eq!(cause, None, "{entry:?}");
        }
        (CheckPlanVerdict::Run, CheckPlanReason::NoInputs) => {
            let check = ours.expect("a check the candidate has");
            assert!(check.inputs.is_empty() && readable().next().is_some());
            assert_eq!(cause, None, "{entry:?}");
        }
        (CheckPlanVerdict::Run, CheckPlanReason::Widened) => {
            assert!(cause.is_some() && entry.widened == cause, "{entry:?}");
        }
        (CheckPlanVerdict::Run, CheckPlanReason::DefinitionChanged) => {
            assert!(matches!(target, Target::Checks(_)), "{entry:?}");
            let change = entry.definition.expect("how the definition changed");
            assert_eq!(ours.is_none(), change == CheckDefinitionChange::Deleted);
            assert_eq!(
                entry.was.is_some(),
                change == CheckDefinitionChange::Renamed
            );
        }
        other => panic!("a verdict and a reason no rule gives: {other:?}"),
    }
}
