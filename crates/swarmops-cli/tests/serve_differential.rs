//! `swarmops serve` against git's own server (the M17 design §2): one scripted session of stock
//! git runs against a served store and against a bare copy holding the same history, and every
//! step's exit status, the refs it reports and the client's objects must be equal, apart from the
//! differences `docs/operate/serve-differences.md` lists with a reason.

use swarmops_testkit::differential::{
    self, BRANCHES, Difference, STORE, Session, Step, TAGS, Transcript, compare, listed,
};
use swarmops_testkit::git::{GitFixture, fixture};
use swarmops_testkit::remote::GitServer;
use swarmops_testkit::serve::ServeSite;

use crate::support::{self, Swarmops};

/// The page that lists the differences.
const DIFFERENCES: &str = include_str!("../../../docs/operate/serve-differences.md");

/// The agent that imports the fixture into the served store.
const WRITER: &str = "agent/worker-1";

/// The fixture both servers hold: branches, merges and a lightweight tag.
const FIXTURE: &str = "merges";

/// One session against each server: serve's transcript, then git's. `fixture` stays alive
/// while they run.
fn transcripts(fixture: &GitFixture) -> (Transcript, Transcript) {
    // 1. The served store, holding the fixture's branches and tags.
    let source = fixture.repo.as_deref().unwrap();
    let site = ServeSite::new::<Swarmops>();
    let store = site.init(STORE);
    let store = store.to_str().unwrap();
    let source_arg = source.to_str().unwrap();
    let imports = BRANCHES
        .iter()
        .map(|branch| (format!("refs/heads/{branch}"), (*branch).to_owned()))
        .chain(
            TAGS.iter()
                .map(|tag| (format!("refs/tags/{tag}"), format!("tags/{tag}"))),
        );
    for (git_ref, into) in imports {
        let line = [
            "--repo", store, "--as", WRITER, "git", "import", source_arg, "--ref", &git_ref,
            "--into", &into,
        ];
        support::run(&line, &[]).ok();
    }

    // 2. The same session against serve and against git's server.
    let session = Session::new(source);
    let serve = session.run_against_serve(&site);
    let git = session.run_against_git(&GitServer::new());
    (serve, git)
}

/// The listed differences without the rows of `command`.
fn without(command: &str) -> Vec<Difference> {
    listed(DIFFERENCES)
        .into_iter()
        .filter(|row| row.command != command)
        .collect()
}

/// Every step answers as git's own server does, apart from the listed differences.
#[test]
fn serve_answers_as_git_does() {
    let fixture = fixture(FIXTURE);
    let (serve, git) = transcripts(&fixture);
    assert_eq!(serve.steps.len(), git.steps.len());
    assert_eq!(serve.steps.len(), differential::script().len());

    // The script does what it says on git's server: the pushes pass, but the forced one.
    for outcome in &git.steps {
        let refused = outcome.step == Step::PushNonFastForward;
        assert_eq!(outcome.exit != 0, refused, "{outcome:?}");
    }
    let failures = compare(&serve, &git, &listed(DIFFERENCES));
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A transcript with one ref changed fails, and the message names the step.
#[test]
fn a_wrong_ref_fails_naming_the_command() {
    let fixture = fixture(FIXTURE);
    let (mut serve, git) = transcripts(&fixture);

    // 1. One ref of the clone's outcome moved to another commit.
    let clone = serve
        .steps
        .iter_mut()
        .find(|outcome| outcome.step == Step::Clone)
        .unwrap();
    let wrong = clone.refs.first_mut().unwrap();
    wrong.1 = "0".repeat(wrong.1.len());
    let name = wrong.0.clone();

    // 2. Reported once, naming the step and the ref.
    let failures = compare(&serve, &git, &listed(DIFFERENCES));
    assert_eq!(failures.len(), 1, "{failures:#?}");
    assert!(failures[0].contains("Clone"), "{}", failures[0]);
    assert!(failures[0].contains(&name), "{}", failures[0]);
}

/// The list without its land-ref rows: the suite reports the land's difference.
#[test]
fn an_unlisted_difference_fails() {
    let fixture = fixture(FIXTURE);
    let (serve, git) = transcripts(&fixture);
    let failures = compare(&serve, &git, &without("PushLand"));
    assert!(!failures.is_empty(), "the land's difference is reported");
    assert!(
        failures.iter().all(|failure| failure.contains("PushLand")),
        "{failures:#?}"
    );
    assert!(
        failures
            .iter()
            .any(|failure| failure.contains("refs/land/main")),
        "{failures:#?}"
    );
}

/// Every listed difference has a reason, and every listed command is a step of the script.
#[test]
fn every_listed_difference_has_a_reason() {
    let rows = listed(DIFFERENCES);
    assert!(
        rows.iter().any(|row| row.command == "PushLand"),
        "{rows:#?}"
    );
    assert!(
        rows.iter().any(|row| row.command == "PushNonFastForward"),
        "{rows:#?}"
    );
    let script = differential::script();
    let names: Vec<&str> = script.iter().map(Step::name).collect();
    for row in &rows {
        assert!(!row.reason.trim().is_empty(), "no reason: {row:?}");
        assert!(!row.difference.trim().is_empty(), "no difference: {row:?}");
        assert!(names.contains(&row.command.as_str()), "not a step: {row:?}");
        assert!(
            ["exit", "refs", "objects"].contains(&row.field.as_str()),
            "not a field: {row:?}"
        );
    }
}
