//! Plans: the rules of `decide` over the fixture's checks, and the forms over a test store and
//! a directory.

use camino::Utf8PathBuf;
use serde_json::json;
use swarmops_api::{
    ChangeInput, ChangeOp, CheckDefinitionChange, CheckPlanOut, CheckPlanReason, CheckPlanReq,
    CheckPlanResp, CheckPlanSubject, CheckPlanVerdict, CheckPlanWidened, CheckStage, CommitReq,
    ContentInput, Identity, OpError, StoreOps,
};

use super::{Given, Matcher, Target, cause_of, decide, plan, plan_dir};
use crate::LocalOps;
use crate::check::changed::{Changed, Sep};
use crate::check::{Check, parse};

/// The target's check file: `docs`, `site` (two stages), `unit` (no inputs), `slow` (manual).
const TARGET: &str = include_str!("../../../../swarmops-cli/tests/fixtures/check/plan/target.toml");

/// The five changed paths of the fixture, one a line, and the same NUL-ended.
const LINES: &[u8] =
    include_bytes!("../../../../swarmops-cli/tests/fixtures/check/plan/changed.txt");
const NULS: &[u8] =
    include_bytes!("../../../../swarmops-cli/tests/fixtures/check/plan/changed.nul");

/// The target's file with one definition changed, each as its name says.
const RENAMED: &str =
    include_str!("../../../../swarmops-cli/tests/fixtures/check/plan/renamed.toml");
const DELETED: &str =
    include_str!("../../../../swarmops-cli/tests/fixtures/check/plan/deleted.toml");
const MODIFIED: &str =
    include_str!("../../../../swarmops-cli/tests/fixtures/check/plan/modified.toml");
const NARROWED: &str =
    include_str!("../../../../swarmops-cli/tests/fixtures/check/plan/narrowed.toml");
const ADDED: &str = include_str!("../../../../swarmops-cli/tests/fixtures/check/plan/added.toml");

/// The checks of the check file `text`.
fn checks(text: &str) -> Vec<Check> {
    parse(text.as_bytes()).unwrap().checks
}

/// The plan of `stage` for the checks of `text` when `paths` changed, in that order.
fn decided(text: &str, stage: CheckStage, paths: &[&str]) -> Vec<CheckPlanOut> {
    let candidate = checks(text);
    let mut matcher = Matcher::new(stage, &candidate);
    let mut changed = Changed::none();
    for path in paths {
        changed.path(path, &mut |path| matcher.see(path));
    }
    decide(stage, &candidate, Target::Unknown, &matcher, &changed)
}

/// The `before-land` plan of the candidate file `candidate` against the target's file `target`
/// when `paths` changed.
fn against(candidate: &str, target: &str, paths: &[&str]) -> Vec<CheckPlanOut> {
    against_at(CheckStage::BeforeLand, candidate, target, paths)
}

/// The same, of `stage`.
fn against_at(
    stage: CheckStage,
    candidate: &str,
    target: &str,
    paths: &[&str],
) -> Vec<CheckPlanOut> {
    let (candidate, target) = (checks(candidate), checks(target));
    let mut matcher = Matcher::new(stage, &candidate);
    let mut changed = Changed::none();
    for path in paths {
        changed.path(path, &mut |path| matcher.see(path));
    }
    decide(
        stage,
        &candidate,
        Target::Checks(&target),
        &matcher,
        &changed,
    )
}

/// The entry of check `name` in `plan`.
#[track_caller]
fn entry<'p>(plan: &'p [CheckPlanOut], name: &str) -> &'p CheckPlanOut {
    plan.iter().find(|check| check.name == name).expect(name)
}

/// The digest of check `name` in the check file `text`.
fn digest_in(text: &str, name: &str) -> String {
    let all = checks(text);
    let check = all.iter().find(|check| check.name == name).unwrap();
    check.definition_digest()
}

/// What a plan says of each check: its name, verdict and reason.
fn verdicts(plan: &[CheckPlanOut]) -> Vec<(&str, CheckPlanVerdict, CheckPlanReason)> {
    (plan.iter())
        .map(|check| (check.name.as_str(), check.verdict, check.reason))
        .collect()
}

const RUN: CheckPlanVerdict = CheckPlanVerdict::Run;
const NOT_APPLICABLE: CheckPlanVerdict = CheckPlanVerdict::NotApplicable;
const INPUT_CHANGED: CheckPlanReason = CheckPlanReason::InputChanged;
const NO_INPUTS: CheckPlanReason = CheckPlanReason::NoInputs;
const INPUTS_UNCHANGED: CheckPlanReason = CheckPlanReason::InputsUnchanged;
const WIDENED: CheckPlanReason = CheckPlanReason::Widened;
const DEFINITION_CHANGED: CheckPlanReason = CheckPlanReason::DefinitionChanged;

/// A changed path among a check's inputs runs it; a check without inputs runs for any path;
/// a check none of whose inputs changed is not applicable: one reason each.
#[test]
fn each_check_has_one_reason_by_its_inputs() {
    let plan = decided(TARGET, CheckStage::BeforeLand, &["crates/a/src/lib.rs"]);
    assert_eq!(
        verdicts(&plan),
        [
            ("docs", NOT_APPLICABLE, INPUTS_UNCHANGED),
            ("site", NOT_APPLICABLE, INPUTS_UNCHANGED),
            ("unit", RUN, NO_INPUTS),
        ]
    );
    let plan = decided(TARGET, CheckStage::BeforeLand, &["web/app/page.tsx"]);
    assert_eq!(
        verdicts(&plan),
        [
            ("docs", NOT_APPLICABLE, INPUTS_UNCHANGED),
            ("site", RUN, INPUT_CHANGED),
            ("unit", RUN, NO_INPUTS),
        ]
    );
    let plan = decided(TARGET, CheckStage::BeforeLand, &["docs/guide.md"]);
    assert_eq!(
        verdicts(&plan),
        [
            ("docs", RUN, INPUT_CHANGED),
            ("site", RUN, INPUT_CHANGED),
            ("unit", RUN, NO_INPUTS),
        ]
    );

    // No path changed: nothing runs, the check without inputs neither.
    let plan = decided(TARGET, CheckStage::BeforeLand, &[]);
    assert_eq!(
        verdicts(&plan),
        [
            ("docs", NOT_APPLICABLE, INPUTS_UNCHANGED),
            ("site", NOT_APPLICABLE, INPUTS_UNCHANGED),
            ("unit", NOT_APPLICABLE, INPUTS_UNCHANGED),
        ]
    );
}

/// An entry holds what its reason has and nothing else; each has its definition's digest.
#[test]
fn an_entry_holds_what_its_reason_has() {
    let candidate = checks(TARGET);
    let plan = decided(TARGET, CheckStage::BeforeLand, &["web/app/page.tsx"]);
    let digest = |name: &str| {
        let check = candidate.iter().find(|check| check.name == name).unwrap();
        check.definition_digest()
    };
    assert_eq!(
        serde_json::to_value(&plan).unwrap(),
        json!([
            {"name": "docs", "verdict": "not_applicable", "reason": "inputs_unchanged",
             "definition_digest": digest("docs")},
            {"name": "site", "verdict": "run", "reason": "input_changed",
             "path": "web/app/page.tsx", "pattern": "web/**",
             "definition_digest": digest("site")},
            {"name": "unit", "verdict": "run", "reason": "no_inputs",
             "definition_digest": digest("unit")},
        ])
    );
}

/// The path named is the first one read that the inputs match, and the pattern the first, in
/// declared order, that matches it.
#[test]
fn the_first_path_and_its_first_pattern_are_named() {
    let both =
        "version = 1\n[check.a]\nargv = [\"x\"]\ninputs = [\"docs/*\", \"docs/**\", \"*\"]\n";
    let named = |paths: &[&str]| {
        let plan = decided(both, CheckStage::BeforeLand, paths);
        (
            plan[0].path.clone().unwrap(),
            plan[0].pattern.clone().unwrap(),
        )
    };
    let pair = |path: &str, pattern: &str| (path.to_owned(), pattern.to_owned());
    assert_eq!(
        named(&["docs/a/b.md", "docs/c.md"]),
        pair("docs/a/b.md", "docs/**")
    );
    assert_eq!(
        named(&["docs/c.md", "docs/a/b.md"]),
        pair("docs/c.md", "docs/*")
    );
    assert_eq!(
        named(&["x/y", "README.md", "docs/c.md"]),
        pair("README.md", "*")
    );
}

/// A plan is of one stage: a check of two stages is in both plans, a manual one in that alone.
#[test]
fn a_plan_is_of_one_stage() {
    let names = |stage| -> Vec<String> {
        (decided(TARGET, stage, &["crates/a/src/lib.rs"]).iter())
            .map(|check| check.name.clone())
            .collect()
    };
    assert_eq!(names(CheckStage::BeforeLand), ["docs", "site", "unit"]);
    assert_eq!(names(CheckStage::Manual), ["site", "slow"]);
    assert_eq!(names(CheckStage::Deploy), Vec::<String>::new());
    let manual = decided(TARGET, CheckStage::Manual, &["crates/a/src/lib.rs"]);
    assert_eq!(
        verdicts(&manual),
        [
            ("site", NOT_APPLICABLE, INPUTS_UNCHANGED),
            ("slow", RUN, INPUT_CHANGED)
        ]
    );
}

/// A widened plan runs every check, also one a path already matched and one no path touches:
/// each names the plan's cause, and no check is left not applicable.
#[test]
fn a_widened_plan_runs_every_check() {
    let candidate = checks(TARGET);
    for cause in [
        CheckPlanWidened::PathsUnknown,
        CheckPlanWidened::PathsTruncated,
        CheckPlanWidened::PathUnreadable,
    ] {
        let mut matcher = Matcher::new(CheckStage::BeforeLand, &candidate);
        let mut changed = Changed::none();
        changed.path("web/app/page.tsx", &mut |path| matcher.see(path));
        changed.widen(cause);
        let stage = CheckStage::BeforeLand;
        let plan = decide(stage, &candidate, Target::Unknown, &matcher, &changed);
        assert_eq!(
            verdicts(&plan),
            [
                ("docs", RUN, WIDENED),
                ("site", RUN, WIDENED),
                ("unit", RUN, WIDENED)
            ],
            "{cause:?}"
        );
        for check in &plan {
            assert_eq!(check.widened, Some(cause), "{check:?}");
            assert_eq!((&check.path, &check.pattern), (&None, &None), "{check:?}");
        }
    }

    // Unknown paths with none read: the same.
    let matcher = Matcher::new(CheckStage::BeforeLand, &candidate);
    let stage = CheckStage::BeforeLand;
    let unknown = Changed::unknown();
    let plan = decide(stage, &candidate, Target::Unknown, &matcher, &unknown);
    assert!(
        plan.iter()
            .all(|check| check.verdict == RUN && check.reason == WIDENED)
    );
}

/// One entry a check of the stage, sorted by name, none twice; and the same twice.
#[test]
fn a_plan_holds_each_check_once_and_is_the_same_twice() {
    let shuffled = "version = 1\n[check.zz]\nargv = [\"x\"]\n[check.a]\nargv = [\"x\"]\n\
                    [check.m]\nargv = [\"x\"]\ninputs = [\"m/**\"]\n";
    let plan = decided(shuffled, CheckStage::BeforeLand, &["m/x", "y"]);
    let names: Vec<&str> = plan.iter().map(|check| check.name.as_str()).collect();
    assert_eq!(names, ["a", "m", "zz"]);
    let again = decided(shuffled, CheckStage::BeforeLand, &["m/x", "y"]);
    assert_eq!(
        serde_json::to_string(&plan).unwrap(),
        serde_json::to_string(&again).unwrap()
    );
}

// Definitions against the target's head.

/// The same file on both sides compares equal: every check is by its inputs, as with no
/// target at all.
#[test]
fn equal_definitions_are_planned_by_their_inputs() {
    let plan = against(TARGET, TARGET, &["web/app/page.tsx"]);
    assert_eq!(
        plan,
        decided(TARGET, CheckStage::BeforeLand, &["web/app/page.tsx"])
    );
    assert!(
        plan.iter()
            .all(|c| c.definition.is_none() && c.was.is_none())
    );
}

/// A definition that differs from the target's under its name is `modified`, and runs
/// whatever changed: also when no path did.
#[test]
fn a_modified_definition_runs() {
    for paths in [&["docs/guide.md"][..], &[]] {
        let plan = against(MODIFIED, TARGET, paths);
        let unit = entry(&plan, "unit");
        assert_eq!(
            (unit.verdict, unit.reason),
            (RUN, DEFINITION_CHANGED),
            "{paths:?}"
        );
        assert_eq!(unit.definition, Some(CheckDefinitionChange::Modified));
        assert_eq!(unit.was, None);
        assert_eq!(unit.definition_digest, digest_in(MODIFIED, "unit"));
        assert_ne!(unit.definition_digest, digest_in(TARGET, "unit"));
        assert_eq!(
            (&unit.path, &unit.pattern, unit.widened),
            (&None, &None, None)
        );
    }

    // The others are by their inputs.
    let plan = against(MODIFIED, TARGET, &[]);
    assert_eq!(
        verdicts(&plan),
        [
            ("docs", NOT_APPLICABLE, INPUTS_UNCHANGED),
            ("site", NOT_APPLICABLE, INPUTS_UNCHANGED),
            ("unit", RUN, DEFINITION_CHANGED),
        ]
    );
}

/// A candidate that narrows a check's own inputs changed its definition: the check runs,
/// though the path that changed is no longer among its inputs.
#[test]
fn narrowed_inputs_are_a_changed_definition() {
    let plan = against(NARROWED, TARGET, &["docs/guide.md"]);
    let site = entry(&plan, "site");
    assert_eq!((site.verdict, site.reason), (RUN, DEFINITION_CHANGED));
    assert_eq!(site.definition, Some(CheckDefinitionChange::Modified));

    // The same paths on the unchanged file run it for its input, and with no target to compare
    // with the narrowed check would not run: narrowing is caught by the comparison alone.
    let unchanged = against(TARGET, TARGET, &["docs/guide.md"]);
    assert_eq!(entry(&unchanged, "site").reason, INPUT_CHANGED);
    let blind = decided(NARROWED, CheckStage::BeforeLand, &["docs/guide.md"]);
    assert_eq!(entry(&blind, "site").verdict, NOT_APPLICABLE);
}

/// A check under a new name with the definition one old name had is `renamed`, `was` that
/// name; the old name is listed too, `deleted`, with the target's digest. Both run.
#[test]
fn a_renamed_definition_runs_under_both_names() {
    let plan = against(RENAMED, TARGET, &[]);
    assert_eq!(
        verdicts(&plan),
        [
            ("docs", RUN, DEFINITION_CHANGED),
            ("prose", RUN, DEFINITION_CHANGED),
            ("site", NOT_APPLICABLE, INPUTS_UNCHANGED),
            ("unit", NOT_APPLICABLE, INPUTS_UNCHANGED),
        ]
    );
    let prose = entry(&plan, "prose");
    assert_eq!(prose.definition, Some(CheckDefinitionChange::Renamed));
    assert_eq!(prose.was.as_deref(), Some("docs"));
    let docs = entry(&plan, "docs");
    assert_eq!(docs.definition, Some(CheckDefinitionChange::Deleted));
    assert_eq!(docs.was, None);
    assert_eq!(docs.definition_digest, prose.definition_digest);
}

/// A rename is told only when the digests pair one to one: two new names of one old
/// definition, or one new name of two old ones, are `added`.
#[test]
fn a_rename_that_does_not_pair_is_added() {
    let one = "version = 1\n[check.a]\nargv = [\"x\"]\n";
    let two_new = "version = 1\n[check.b]\nargv = [\"x\"]\n[check.c]\nargv = [\"x\"]\n";
    let plan = against(two_new, one, &[]);
    for name in ["b", "c"] {
        let check = entry(&plan, name);
        assert_eq!(
            check.definition,
            Some(CheckDefinitionChange::Added),
            "{name}"
        );
        assert_eq!(check.was, None, "{name}");
    }
    let a = entry(&plan, "a");
    assert_eq!(a.definition, Some(CheckDefinitionChange::Deleted));

    let two_old = "version = 1\n[check.a]\nargv = [\"x\"]\n[check.b]\nargv = [\"x\"]\n";
    let one_new = "version = 1\n[check.c]\nargv = [\"x\"]\n";
    let plan = against(one_new, two_old, &[]);
    let c = entry(&plan, "c");
    assert_eq!(c.definition, Some(CheckDefinitionChange::Added));

    // A check both sides keep under one name takes no part: beside it, a pair of one digest is
    // still a rename.
    let was_b = "version = 1\n[check.a]\nargv = [\"x\"]\n[check.b]\nargv = [\"x\"]\n";
    let now_c = "version = 1\n[check.a]\nargv = [\"x\"]\n[check.c]\nargv = [\"x\"]\n";
    let plan = against(now_c, was_b, &[]);
    let c = entry(&plan, "c");
    assert_eq!(c.definition, Some(CheckDefinitionChange::Renamed));
    assert_eq!(c.was.as_deref(), Some("b"));
    assert_eq!(entry(&plan, "a").verdict, NOT_APPLICABLE);

    // A name both sides keep is no rename's old name, though its digest is the new check's.
    let kept = "version = 1\n[check.a]\nargv = [\"x\"]\n[check.b]\nargv = [\"x\"]\n";
    let plan = against(kept, one, &[]);
    let b = entry(&plan, "b");
    assert_eq!(b.definition, Some(CheckDefinitionChange::Added));
    assert_eq!(entry(&plan, "a").verdict, NOT_APPLICABLE);
}

/// A check the target's head does not have is `added`; one the candidate no longer has is
/// listed `deleted`, with the target's digest, though the candidate's file cannot run it.
#[test]
fn an_added_and_a_deleted_definition_run() {
    let plan = against(ADDED, TARGET, &[]);
    let lint = entry(&plan, "lint");
    assert_eq!((lint.verdict, lint.reason), (RUN, DEFINITION_CHANGED));
    assert_eq!(lint.definition, Some(CheckDefinitionChange::Added));
    assert_eq!(lint.was, None);

    let plan = against(DELETED, TARGET, &[]);
    assert_eq!(
        verdicts(&plan),
        [
            ("docs", NOT_APPLICABLE, INPUTS_UNCHANGED),
            ("site", RUN, DEFINITION_CHANGED),
            ("unit", NOT_APPLICABLE, INPUTS_UNCHANGED),
        ]
    );
    let site = entry(&plan, "site");
    assert_eq!(site.definition, Some(CheckDefinitionChange::Deleted));
    assert_eq!(site.definition_digest, digest_in(TARGET, "site"));
}

/// Membership of the planned stage is what is compared: a check the candidate moved out of
/// the stage is `deleted` for it, one moved in is `added`; `stages` is not in the digest.
#[test]
fn a_check_moved_between_stages_is_deleted_and_added() {
    let moved = TARGET.replace(
        "argv = [\"cargo\", \"test\", \"--locked\"]\n\n[check.slow]",
        "argv = [\"cargo\", \"test\", \"--locked\"]\nstages = [\"manual\"]\n\n[check.slow]",
    );
    assert_ne!(moved, TARGET);
    assert_eq!(digest_in(&moved, "unit"), digest_in(TARGET, "unit"));
    let before = against(&moved, TARGET, &[]);
    let unit = entry(&before, "unit");
    assert_eq!(unit.verdict, RUN);
    assert_eq!(unit.definition, Some(CheckDefinitionChange::Deleted));
    let manual = against_at(CheckStage::Manual, &moved, TARGET, &[]);
    let unit = entry(&manual, "unit");
    assert_eq!(unit.verdict, RUN);
    assert_eq!(unit.definition, Some(CheckDefinitionChange::Added));
}

/// A side with no check: every check of the other is `added`, or `deleted`; neither side
/// with a check of the stage is a plan of none.
#[test]
fn a_side_without_checks_adds_or_deletes_them_all() {
    let none = "version = 1\n";
    let added = against(TARGET, none, &[]);
    assert_eq!(added.len(), 3);
    assert!(
        added
            .iter()
            .all(|c| c.definition == Some(CheckDefinitionChange::Added))
    );
    let deleted = against(none, TARGET, &[]);
    let names: Vec<&str> = deleted.iter().map(|check| check.name.as_str()).collect();
    assert_eq!(names, ["docs", "site", "unit"], "not an empty plan");
    assert!(deleted.iter().all(|c| c.verdict == RUN));
    assert!(
        deleted
            .iter()
            .all(|c| c.definition == Some(CheckDefinitionChange::Deleted))
    );
    assert_eq!(against(none, none, &[]), []);
    assert_eq!(against_at(CheckStage::Deploy, TARGET, TARGET, &[]), []);
}

/// The order of the rules: a changed definition is told before a widened plan, and a widened
/// plan before a match. A check whose definition changed is never not applicable.
#[test]
fn a_changed_definition_comes_before_a_widened_plan() {
    let (candidate, target) = (checks(MODIFIED), checks(TARGET));
    let stage = CheckStage::BeforeLand;
    let mut matcher = Matcher::new(stage, &candidate);
    let mut changed = Changed::unknown();
    changed.path("docs/guide.md", &mut |path| matcher.see(path));
    let plan = decide(
        stage,
        &candidate,
        Target::Checks(&target),
        &matcher,
        &changed,
    );
    assert_eq!(
        verdicts(&plan),
        [
            ("docs", RUN, WIDENED),
            ("site", RUN, WIDENED),
            ("unit", RUN, DEFINITION_CHANGED)
        ]
    );
    assert_eq!(entry(&plan, "unit").widened, None);

    // Whatever changed, and whichever file the candidate has, no changed definition is left
    // not applicable.
    for file in [RENAMED, DELETED, MODIFIED, NARROWED, ADDED] {
        for paths in [&[][..], &["x"], &["docs/a.md", "web/b", "crates/c"]] {
            let plan = against(file, TARGET, paths);
            for check in &plan {
                let differs = check.definition.is_some();
                assert_eq!(differs, check.reason == DEFINITION_CHANGED, "{check:?}");
                assert!(!differs || check.verdict == RUN, "{check:?}");
            }
            assert!(plan.iter().any(|check| check.definition.is_some()));
        }
    }
}

/// A target whose check file does not read widens the whole plan, `target_unread`: the
/// candidate's checks are listed, every one `run`. A cause of the paths comes before it.
#[test]
fn an_unread_target_widens_the_plan() {
    let candidate = checks(TARGET);
    let stage = CheckStage::BeforeLand;
    let matcher = Matcher::new(stage, &candidate);
    let unread = Some(CheckPlanWidened::TargetUnread);
    let plan = decide(
        stage,
        &candidate,
        Target::Unread,
        &matcher,
        &Changed::none(),
    );
    assert_eq!(
        verdicts(&plan),
        [
            ("docs", RUN, WIDENED),
            ("site", RUN, WIDENED),
            ("unit", RUN, WIDENED)
        ]
    );
    assert!(plan.iter().all(|c| c.widened == unread));
    assert_eq!(cause_of(&Target::Unread, &Changed::none()), unread);

    let unknown = Some(CheckPlanWidened::PathsUnknown);
    let plan = decide(
        stage,
        &candidate,
        Target::Unread,
        &matcher,
        &Changed::unknown(),
    );
    assert!(plan.iter().all(|c| c.widened == unknown));
    assert_eq!(cause_of(&Target::Unread, &Changed::unknown()), unknown);
    assert_eq!(cause_of(&Target::Unknown, &Changed::none()), None);
    assert_eq!(
        cause_of(&Target::Checks(&candidate), &Changed::none()),
        None
    );
}

/// A plan holds exactly one entry for each name either side has in the stage, sorted.
#[test]
fn a_plan_holds_one_entry_for_each_name_of_either_side() {
    let files = [
        TARGET,
        RENAMED,
        DELETED,
        MODIFIED,
        NARROWED,
        ADDED,
        "version = 1\n",
    ];
    for candidate in files {
        for target in files {
            for stage in [CheckStage::BeforeLand, CheckStage::Manual] {
                let plan = against_at(stage, candidate, target, &["docs/guide.md"]);
                let mut expected: Vec<String> = (checks(candidate).iter())
                    .chain(checks(target).iter())
                    .filter(|check| check.is_of(stage))
                    .map(|check| check.name.clone())
                    .collect();
                expected.sort();
                expected.dedup();
                let names: Vec<String> = plan.iter().map(|c| c.name.clone()).collect();
                assert_eq!(names, expected, "{stage}");
            }
        }
    }
}

// The store form.

/// A store whose `main` holds the fixture's check file and one file of each kind its checks
/// name; the identity that commits, and `main`'s head.
fn store() -> (tempfile::TempDir, LocalOps, Identity, String) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    let (ops, _) = LocalOps::init(&root, "test").unwrap();
    let who = Identity::parse("agent/test").unwrap();
    let files = [
        (".swarmops/checks.toml", TARGET),
        ("README.md", "readme\n"),
        ("crates/a/src/lib.rs", "lib\n"),
        ("docs/guide.md", "guide\n"),
        ("web/app/page.tsx", "page\n"),
    ];
    let mut req = CommitReq::new(
        files.iter().map(|(p, c)| put(p, c)).collect(),
        "root".to_owned(),
    );
    req.advance = Some("main".to_owned());
    let head = ops.commit(&who, req).unwrap().changeset;
    (dir, ops, who, head)
}

/// A change that puts `content` at `path`.
fn put(path: &str, content: &str) -> ChangeInput {
    let mut change = ChangeInput::new(path.to_owned(), ChangeOp::Put);
    change.content = Some(ContentInput::Inline(content.to_owned()));
    change
}

/// A changeset of `changes` on `parent`, moving no ref.
fn commit(ops: &LocalOps, who: &Identity, parent: &str, changes: Vec<ChangeInput>) -> String {
    let mut req = CommitReq::new(changes, "change".to_owned());
    req.parent = Some(parent.into());
    ops.commit(who, req).unwrap().changeset
}

/// The error `result` holds, or the test fails.
#[track_caller]
fn error_of(result: Result<CheckPlanResp, OpError>) -> OpError {
    result.expect_err("an error")
}

/// A changeset against its target: the paths it changed since their merge base, the tree, the
/// base and the target's head named, the file as `check list` names it.
#[test]
fn a_changeset_is_planned_against_its_target() {
    let (_dir, ops, who, main) = store();
    let first = commit(&ops, &who, &main, vec![put("docs/guide.md", "more\n")]);
    let change = commit(
        &ops,
        &who,
        &first,
        vec![
            put("docs/new.md", "new\n"),
            ChangeInput::new("web/app/page.tsx".to_owned(), ChangeOp::Delete),
        ],
    );
    let resp = plan(&ops, CheckPlanReq::new(change.as_str())).unwrap();
    assert_eq!(
        (resp.vouched, resp.subject, resp.stage),
        (false, CheckPlanSubject::Changeset, CheckStage::BeforeLand)
    );
    assert_eq!(resp.rev.as_deref(), Some(change.as_str()));
    assert_eq!(
        resp.tree,
        Some(
            ops.show(swarmops_api::ShowReq::new(change.as_str()))
                .unwrap()
                .root
        )
    );
    assert_eq!(resp.target.as_deref(), Some("main"));
    assert_eq!(resp.onto.as_deref(), Some(main.as_str()));
    assert_eq!(resp.base.as_deref(), Some(main.as_str()));
    assert_eq!(
        (resp.file.as_deref(), resp.version),
        (Some(".swarmops/checks.toml"), Some(1))
    );
    assert_eq!(
        (resp.changed, resp.widened),
        (3, None),
        "two commits, three paths"
    );
    assert_eq!(
        (resp.workspace, resp.edits, resp.outcome),
        (None, None, None)
    );
    assert_eq!(
        verdicts(&resp.checks),
        [
            ("docs", RUN, INPUT_CHANGED),
            ("site", RUN, INPUT_CHANGED),
            ("unit", RUN, NO_INPUTS)
        ]
    );

    // The first changed path in path order, and a deleted file is a changed path.
    assert_eq!(resp.checks[0].path.as_deref(), Some("docs/guide.md"));
    assert_eq!(resp.checks[1].path.as_deref(), Some("docs/guide.md"));
    let only_web = commit(
        &ops,
        &who,
        &main,
        vec![ChangeInput::new(
            "web/app/page.tsx".to_owned(),
            ChangeOp::Delete,
        )],
    );
    let resp = plan(&ops, CheckPlanReq::new(only_web.as_str())).unwrap();
    assert_eq!(
        verdicts(&resp.checks),
        [
            ("docs", NOT_APPLICABLE, INPUTS_UNCHANGED),
            ("site", RUN, INPUT_CHANGED),
            ("unit", RUN, NO_INPUTS),
        ]
    );
    assert_eq!(resp.checks[1].pattern.as_deref(), Some("web/**"));

    // The target itself changes nothing against itself.
    let resp = plan(&ops, CheckPlanReq::new("main")).unwrap();
    assert_eq!(resp.changed, 0);
    assert!(
        resp.checks
            .iter()
            .all(|check| check.verdict == NOT_APPLICABLE)
    );
}

/// The changed paths are those since the merge base: what the target gained since is not the
/// candidate's change.
#[test]
fn the_changed_paths_are_those_since_the_merge_base() {
    let (_dir, ops, who, main) = store();
    let change = commit(
        &ops,
        &who,
        &main,
        vec![put("crates/a/src/lib.rs", "edit\n")],
    );
    let mut moved = CommitReq::new(vec![put("docs/guide.md", "target\n")], "target".to_owned());
    moved.parent = Some(main.as_str().into());
    moved.advance = Some("main".to_owned());
    let onto = ops.commit(&who, moved).unwrap().changeset;
    let resp = plan(&ops, CheckPlanReq::new(change.as_str())).unwrap();
    assert_eq!(resp.base.as_deref(), Some(main.as_str()));
    assert_eq!(resp.onto.as_deref(), Some(onto.as_str()));
    assert_eq!(resp.changed, 1);
    assert_eq!(
        verdicts(&resp.checks),
        [
            ("docs", NOT_APPLICABLE, INPUTS_UNCHANGED),
            ("site", NOT_APPLICABLE, INPUTS_UNCHANGED),
            ("unit", RUN, NO_INPUTS),
        ]
    );
}

/// A candidate that shares no history with its target has no base: its changed paths are not
/// known, and every check runs.
#[test]
fn no_shared_history_widens_the_plan() {
    let (_dir, ops, who, _main) = store();
    let mut root = CommitReq::new(
        vec![put(".swarmops/checks.toml", TARGET), put("other", "x\n")],
        "another root".to_owned(),
    );
    root.advance = Some("release/other".to_owned());
    let other = ops.commit(&who, root).unwrap().changeset;
    let resp = plan(&ops, CheckPlanReq::new(other.as_str())).unwrap();
    assert_eq!(resp.base, None);
    assert_eq!(
        (resp.changed, resp.widened),
        (0, Some(CheckPlanWidened::PathsUnknown))
    );
    assert_eq!(
        verdicts(&resp.checks),
        [
            ("docs", RUN, WIDENED),
            ("site", RUN, WIDENED),
            ("unit", RUN, WIDENED)
        ]
    );
    assert!(
        resp.checks
            .iter()
            .all(|c| c.widened == Some(CheckPlanWidened::PathsUnknown))
    );
}

/// A plan of more paths than one page of the store's diff reads them all.
#[test]
fn a_change_of_many_paths_is_read_to_its_end() {
    let (_dir, ops, who, main) = store();
    let many: Vec<ChangeInput> = (0..1_500)
        .map(|i| put(&format!("data/f{i:04}"), "x\n"))
        .collect();
    let mut changes = many;
    changes.push(put("web/z.tsx", "last\n"));
    let change = commit(&ops, &who, &main, changes);
    let resp = plan(&ops, CheckPlanReq::new(change.as_str())).unwrap();
    assert_eq!((resp.changed, resp.widened), (1_501, None));
    assert_eq!(
        resp.checks[1].path.as_deref(),
        Some("web/z.tsx"),
        "past the first page"
    );
}

/// The definitions are compared with the target's head, not with the merge base: a candidate
/// that changed a definition is `modified`, and so is one that left it alone while the target
/// changed it since.
#[test]
fn a_definition_is_compared_with_the_target_s_head() {
    // 1. The candidate changed `unit`'s argv.
    let (_dir, ops, who, main) = store();
    let change = commit(
        &ops,
        &who,
        &main,
        vec![put(".swarmops/checks.toml", MODIFIED)],
    );
    let resp = plan(&ops, CheckPlanReq::new(change.as_str())).unwrap();
    assert_eq!(resp.widened, None);
    let unit = entry(&resp.checks, "unit");
    assert_eq!((unit.verdict, unit.reason), (RUN, DEFINITION_CHANGED));
    assert_eq!(unit.definition, Some(CheckDefinitionChange::Modified));
    assert_eq!(entry(&resp.checks, "docs").verdict, NOT_APPLICABLE);

    // 2. The candidate touched a crate file alone; the target changed `unit` since the base.
    let aside = commit(
        &ops,
        &who,
        &main,
        vec![put("crates/a/src/lib.rs", "edit\n")],
    );
    let before = plan(&ops, CheckPlanReq::new(aside.as_str())).unwrap();
    assert_eq!(entry(&before.checks, "unit").reason, NO_INPUTS);
    let mut moved = CommitReq::new(vec![put(".swarmops/checks.toml", MODIFIED)], "t".to_owned());
    moved.parent = Some(main.as_str().into());
    moved.advance = Some("main".to_owned());
    let onto = ops.commit(&who, moved).unwrap().changeset;
    let after = plan(&ops, CheckPlanReq::new(aside.as_str())).unwrap();
    assert_eq!(after.onto.as_deref(), Some(onto.as_str()));
    assert_eq!(after.base.as_deref(), Some(main.as_str()));
    let unit = entry(&after.checks, "unit");
    assert_eq!((unit.verdict, unit.reason), (RUN, DEFINITION_CHANGED));
    assert_eq!(unit.definition, Some(CheckDefinitionChange::Modified));
    assert_eq!(unit.definition_digest, digest_in(TARGET, "unit"));
}

/// Renamed, deleted, added and narrowed, each through the store.
#[test]
fn each_definition_change_is_told_through_the_store() {
    let (_dir, ops, who, main) = store();
    let with = |file: &str, more: Vec<ChangeInput>| {
        let mut changes = vec![put(".swarmops/checks.toml", file)];
        changes.extend(more);
        let change = commit(&ops, &who, &main, changes);
        plan(&ops, CheckPlanReq::new(change.as_str())).unwrap()
    };
    let renamed = with(RENAMED, Vec::new());
    let prose = entry(&renamed.checks, "prose");
    assert_eq!(prose.definition, Some(CheckDefinitionChange::Renamed));
    assert_eq!(prose.was.as_deref(), Some("docs"));
    let docs = entry(&renamed.checks, "docs");
    assert_eq!(docs.definition, Some(CheckDefinitionChange::Deleted));

    let deleted = with(DELETED, Vec::new());
    let site = entry(&deleted.checks, "site");
    assert_eq!(site.definition, Some(CheckDefinitionChange::Deleted));
    let added = with(ADDED, Vec::new());
    let lint = entry(&added.checks, "lint");
    assert_eq!(lint.definition, Some(CheckDefinitionChange::Added));

    let narrowed = with(NARROWED, vec![put("docs/x.md", "x\n")]);
    let site = entry(&narrowed.checks, "site");
    assert_eq!((site.verdict, site.reason), (RUN, DEFINITION_CHANGED));
    assert_eq!(site.definition, Some(CheckDefinitionChange::Modified));

    // A candidate that deletes its check file lists the target's checks as deleted.
    let delete = ChangeInput::new(".swarmops/checks.toml".to_owned(), ChangeOp::Delete);
    let gone = commit(&ops, &who, &main, vec![delete]);
    let resp = plan(&ops, CheckPlanReq::new(gone.as_str())).unwrap();
    assert_eq!((resp.file, resp.version), (None, None));
    let names: Vec<&str> = resp
        .checks
        .iter()
        .map(|check| check.name.as_str())
        .collect();
    assert_eq!(names, ["docs", "site", "unit"]);
    assert!(
        resp.checks
            .iter()
            .all(|c| c.definition == Some(CheckDefinitionChange::Deleted))
    );

    // A deleted check may be asked for by name: the plan holds it.
    let mut req = CheckPlanReq::new(gone.as_str());
    req.checks = vec!["site".to_owned()];
    assert_eq!(plan(&ops, req).unwrap().checks.len(), 1);
}

/// The target's check file does not read: no comparison can be made, so the whole plan is
/// widened and the candidate's checks all run. The candidate's own file not reading is the
/// error.
#[test]
fn a_target_whose_file_does_not_read_widens_the_plan() {
    let (_dir, ops, who, main) = store();
    let change = commit(
        &ops,
        &who,
        &main,
        vec![put("crates/a/src/lib.rs", "edit\n")],
    );
    let broken = vec![put(".swarmops/checks.toml", "version = 2\n")];
    let mut broken = CommitReq::new(broken, "break".to_owned());
    broken.parent = Some(main.as_str().into());
    broken.advance = Some("main".to_owned());
    ops.commit(&who, broken).unwrap();
    let resp = plan(&ops, CheckPlanReq::new(change.as_str())).unwrap();
    let unread = Some(CheckPlanWidened::TargetUnread);
    assert_eq!((resp.changed, resp.widened), (1, unread));
    assert_eq!(
        verdicts(&resp.checks),
        [
            ("docs", RUN, WIDENED),
            ("site", RUN, WIDENED),
            ("unit", RUN, WIDENED)
        ]
    );
    assert!(resp.checks.iter().all(|c| c.widened == unread));

    let error = error_of(plan(&ops, CheckPlanReq::new("main")));
    assert_eq!(error.code, "E_CHECK_FILE", "the candidate's: {error:?}");
}

/// An error is no plan: a revision or a target that names nothing, a candidate whose check
/// file does not read.
#[test]
fn an_error_is_no_plan() {
    let (_dir, ops, who, main) = store();
    let error = error_of(plan(&ops, CheckPlanReq::new("nothing")));
    assert_eq!(error.code, "E_REV_NOT_FOUND", "{error:?}");
    let mut req = CheckPlanReq::new("main");
    req.target = "release/none".to_owned();
    let error = error_of(plan(&ops, req));
    assert!(
        error.code == "E_REV_NOT_FOUND" || error.code == "E_REF_NOT_FOUND",
        "{error:?}"
    );

    let broken = commit(
        &ops,
        &who,
        &main,
        vec![put(".swarmops/checks.toml", "version = 2\n")],
    );
    let error = error_of(plan(&ops, CheckPlanReq::new(broken.as_str())));
    assert_eq!(error.code, "E_CHECK_FILE", "{error:?}");
    assert_eq!(error.details["vouched"], false);
}

/// `checks` keeps the checks named; a name the plan does not hold is refused: `stage` for a
/// check of another stage, with its stages, else `check`.
#[test]
fn only_the_checks_named_are_given() {
    let (_dir, ops, _who, _main) = store();
    let mut req = CheckPlanReq::new("main");
    req.checks = vec!["unit".to_owned(), "docs".to_owned(), "unit".to_owned()];
    let resp = plan(&ops, req).unwrap();
    let names: Vec<&str> = resp
        .checks
        .iter()
        .map(|check| check.name.as_str())
        .collect();
    assert_eq!(names, ["docs", "unit"]);

    let mut req = CheckPlanReq::new("main");
    req.checks = vec!["slow".to_owned()];
    let error = error_of(plan(&ops, req));
    assert_eq!(error.code, "E_INVALID_ARG", "{error:?}");
    assert_eq!(error.details["reason"], "stage");
    assert_eq!(error.details["check"], "slow");
    assert_eq!(error.details["stages"], json!(["manual"]));
    assert_eq!(
        error.hint.as_deref(),
        Some("give the check's stage: --stage manual")
    );

    let mut req = CheckPlanReq::new("main");
    req.checks = vec!["nope".to_owned()];
    let error = error_of(plan(&ops, req));
    assert_eq!(error.code, "E_INVALID_ARG", "{error:?}");
    assert_eq!(
        (&error.details["reason"], &error.details["check"]),
        (&json!("check"), &json!("nope"))
    );

    let mut req = CheckPlanReq::new("main");
    req.stage = CheckStage::Manual;
    req.checks = vec!["slow".to_owned()];
    assert_eq!(plan(&ops, req).unwrap().checks.len(), 1);
}

// The directory form.

/// A directory that holds the fixture's check file.
fn dir() -> (tempfile::TempDir, Utf8PathBuf) {
    let held = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(held.path().to_owned()).unwrap();
    std::fs::create_dir(root.join(".swarmops")).unwrap();
    std::fs::write(root.join(".swarmops/checks.toml"), TARGET).unwrap();
    (held, root)
}

/// The plan of the fixture's directory for `flags` and `list`.
fn planned(flags: &[&str], list: Option<(&[u8], Sep)>) -> CheckPlanResp {
    let (_held, root) = dir();
    let flags: Vec<String> = flags.iter().map(|flag| (*flag).to_owned()).collect();
    let given = Given::Paths {
        flags: &flags,
        list,
    };
    plan_dir(&root, CheckStage::BeforeLand, &[], given).unwrap()
}

/// The directory form is vouched, names no tree, no revision and no target; paths come from
/// the flags, then from the list, in the caller's order.
#[test]
fn the_directory_form_plans_the_paths_given() {
    let resp = planned(&["web/app/page.tsx"], None);
    assert_eq!(
        (resp.vouched, resp.subject, resp.stage),
        (true, CheckPlanSubject::Paths, CheckStage::BeforeLand)
    );
    assert_eq!(
        (&resp.rev, &resp.tree, &resp.target, &resp.onto, &resp.base),
        (&None, &None, &None, &None, &None)
    );
    assert_eq!(
        (resp.file.as_deref(), resp.version),
        (Some(".swarmops/checks.toml"), Some(1))
    );
    assert_eq!((resp.changed, resp.widened), (1, None));
    assert_eq!(
        verdicts(&resp.checks),
        [
            ("docs", NOT_APPLICABLE, INPUTS_UNCHANGED),
            ("site", RUN, INPUT_CHANGED),
            ("unit", RUN, NO_INPUTS),
        ]
    );

    // A list of lines and the same NUL-ended give the same plan; flags are read first.
    let lines = planned(&[], Some((LINES, Sep::Line)));
    let nuls = planned(&[], Some((NULS, Sep::Nul)));
    assert_eq!(lines, nuls);
    assert_eq!(lines.changed, 5);
    assert_eq!(
        lines.checks[0].path.as_deref(),
        Some("docs/status/M1/a b.txt")
    );
    let both = planned(&["README.md"], Some((LINES, Sep::Line)));
    assert_eq!(both.changed, 6);
    assert_eq!(
        both.checks[0].path.as_deref(),
        Some("README.md"),
        "the flags' paths first"
    );
    assert_eq!(both.checks[0].pattern.as_deref(), Some("README.md"));
}

/// No path given is not a change of no path: the first is unknown and runs every check, the
/// second (an empty list) runs none.
#[test]
fn no_path_given_is_unknown_and_an_empty_list_is_no_change() {
    let (_held, root) = dir();
    let unknown = plan_dir(&root, CheckStage::BeforeLand, &[], Given::None).unwrap();
    assert_eq!(
        (unknown.changed, unknown.widened),
        (0, Some(CheckPlanWidened::PathsUnknown))
    );
    assert_eq!(
        verdicts(&unknown.checks),
        [
            ("docs", RUN, WIDENED),
            ("site", RUN, WIDENED),
            ("unit", RUN, WIDENED)
        ]
    );

    let empty = planned(&[], Some((b"", Sep::Line)));
    assert_eq!((empty.changed, empty.widened), (0, None));
    assert!(
        empty
            .checks
            .iter()
            .all(|check| check.verdict == NOT_APPLICABLE)
    );
    let none = planned(&[], None);
    assert_eq!(none, empty, "the flags given, none of them");
}

/// A path given that does not read widens the plan, from a flag as from a list.
#[test]
fn an_unreadable_path_given_widens_the_plan() {
    let unreadable = Some(CheckPlanWidened::PathUnreadable);
    assert_eq!(planned(&["../x"], None).widened, unreadable);
    assert_eq!(planned(&["/abs"], None).widened, unreadable);
    let quoted = planned(&[], Some((b"\"docs/a b.md\"\n", Sep::Line)));
    assert_eq!(quoted.widened, unreadable);
    assert!(
        quoted
            .checks
            .iter()
            .all(|check| check.verdict == RUN && check.reason == WIDENED)
    );
    assert_eq!(
        planned(&[], Some((b"a\xffb\0", Sep::Nul))).widened,
        unreadable
    );
}

/// The directory form has no target to compare with: when the check file, the legacy file or
/// `.swarmops` is among the changed paths, every check runs, `check_file_changed`; a file
/// beside them changes nothing of it, and no check is ever `definition_changed` here.
#[test]
fn a_changed_check_file_widens_the_directory_form() {
    let file_changed = Some(CheckPlanWidened::CheckFileChanged);
    for path in [
        ".swarmops/checks.toml",
        ".swarmops/workspaces.json",
        ".swarmops",
    ] {
        let resp = planned(&["crates/a/src/lib.rs", path], None);
        assert_eq!(resp.widened, file_changed, "{path}");
        assert_eq!(
            verdicts(&resp.checks),
            [
                ("docs", RUN, WIDENED),
                ("site", RUN, WIDENED),
                ("unit", RUN, WIDENED)
            ],
            "{path}"
        );
        assert!(
            resp.checks.iter().all(|c| c.widened == file_changed),
            "{path}"
        );
    }
    let beside = planned(&[".swarmops/access.toml"], None);
    assert_eq!(beside.widened, None);
    assert!(beside.checks.iter().all(|check| check.definition.is_none()));

    // An unreadable path beside it is the cause named: it comes first in the causes' order.
    let both = planned(&[".swarmops/checks.toml", "../x"], None);
    assert_eq!(both.widened, Some(CheckPlanWidened::PathUnreadable));
}

/// The directory form's errors are no plan: a check file that does not read is vouched, a
/// directory that is none is refused, and the stage and check refusals are the store form's.
#[test]
fn the_directory_form_s_errors_are_no_plan() {
    let (_held, root) = dir();
    std::fs::write(root.join(".swarmops/checks.toml"), "version = 2\n").unwrap();
    let error = error_of(plan_dir(&root, CheckStage::BeforeLand, &[], Given::None));
    assert_eq!(error.code, "E_CHECK_FILE", "{error:?}");
    assert_eq!(error.details["vouched"], true);
    let error = error_of(plan_dir(
        &root.join("none"),
        CheckStage::BeforeLand,
        &[],
        Given::None,
    ));
    assert_eq!(
        (error.code.as_str(), &error.details["reason"]),
        ("E_INVALID_ARG", &json!("dir"))
    );

    let (_held, root) = dir();
    let slow = ["slow".to_owned()];
    let error = error_of(plan_dir(&root, CheckStage::BeforeLand, &slow, Given::None));
    assert_eq!(error.details["reason"], "stage");
    let manual = plan_dir(&root, CheckStage::Manual, &slow, Given::None).unwrap();
    assert_eq!(manual.checks.len(), 1);

    // A directory with no check file plans no check, and that is a plan.
    let bare = tempfile::tempdir().unwrap();
    let bare = Utf8PathBuf::from_path_buf(bare.path().to_owned()).unwrap();
    let none = plan_dir(&bare, CheckStage::BeforeLand, &[], Given::None).unwrap();
    assert_eq!((none.checks.len(), none.file), (0, None));
}
