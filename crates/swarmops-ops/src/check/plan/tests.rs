//! Plans: the rules of `decide` over the fixture's checks, and the forms over a test store and
//! a directory.

use camino::Utf8PathBuf;
use serde_json::json;
use swarmops_api::{
    ChangeInput, ChangeOp, CheckPlanOut, CheckPlanReason, CheckPlanReq, CheckPlanResp,
    CheckPlanSubject, CheckPlanVerdict, CheckPlanWidened, CheckStage, CommitReq, ContentInput,
    Identity, OpError, StoreOps,
};

use super::{Given, Matcher, decide, plan, plan_dir};
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
    decide(stage, &candidate, &matcher, &changed)
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
        let plan = decide(CheckStage::BeforeLand, &candidate, &matcher, &changed);
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
    let plan = decide(
        CheckStage::BeforeLand,
        &candidate,
        &matcher,
        &Changed::unknown(),
    );
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
