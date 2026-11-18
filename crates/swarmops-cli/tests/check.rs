//! `check list` (M30-1, M30-2) through the binary: the same tree gives the same answer byte for
//! byte, a tree with no file lists nothing, the directory form says the caller vouched, the
//! one check `default` is the command `ws land --check` runs, and no link is followed.

use std::fs;
use std::path::Path;

use serde_json::{Value, json};

use crate::ws::{Lab, Out, inside, main_of, run_in, write};

/// The plan's fixture, and the same file with a key version 1 does not take on line 11.
const FIXTURE: &str = include_str!("fixtures/check/checks.toml");
const REFUSED: &str = include_str!("fixtures/check/refused.toml");

/// The `result` of `out`'s envelope as printed, byte for byte: what follows `"result":` up to
/// `,"meta":`, which the envelope writes after it.
fn result_text(out: &Out) -> String {
    assert_eq!(out.exit, 0, "{out}");
    let start = out.stdout.find(r#""result":"#).expect("a result") + r#""result":"#.len();
    let end = out.stdout.rfind(r#","meta":"#).expect("a meta");
    out.stdout[start..end].to_owned()
}

/// Commits `changes` (a JSON array) on main's head, as the lab's agent, and moves main there; the
/// changeset.
fn publish_changes(lab: &Lab, changes: &Value) -> String {
    let file = lab.beside("changes.json");
    fs::write(&file, changes.to_string()).unwrap();
    let before = main_of(lab);
    let line = ["commit", "-m", "c", "--changes", file.to_str().unwrap()];
    let committed = lab.so(&[&line[..], &["--parent", &before, "--advance", "main"]].concat());
    committed.ok()["changeset"].as_str().unwrap().to_owned()
}

/// `check list --dir dir`, run beside the store with no store named.
fn list_dir(lab: &Lab, dir: &Path) -> Out {
    run_in(
        lab.store.tmp(),
        &[],
        &["check", "list", "--dir", dir.to_str().unwrap()],
        None,
    )
}

/// Two changesets with the same root tree give the same `result`, byte for byte; so do two runs
/// of the directory form over the same files.
#[test]
fn the_same_tree_gives_the_same_answer() {
    let lab = Lab::new();
    lab.publish(".swarmops/checks.toml", FIXTURE);
    let first = main_of(&lab);
    lab.publish("README.md", "# changed\n");
    lab.publish("README.md", "# acme\n");
    let again = main_of(&lab);
    assert_ne!(first, again, "two changesets");

    let at_first = lab.so(&["check", "list", &first]);
    let at_again = lab.so(&["check", "list", &again]);
    assert_eq!(result_text(&at_first), result_text(&at_again));
    assert_eq!(at_first.ok()["checks"].as_array().unwrap().len(), 3);

    let dir = lab.beside("tree");
    write(&dir, ".swarmops/checks.toml", FIXTURE);
    let once = list_dir(&lab, &dir);
    let twice = list_dir(&lab, &dir);
    assert_eq!(result_text(&once), result_text(&twice));
    assert_eq!(once.ok()["checks"], at_first.ok()["checks"]);
}

/// A tree with no check file, in the store or in a directory, lists no check and exits 0.
#[test]
fn no_file_lists_nothing() {
    let lab = Lab::new();
    let stored = lab.so(&["check", "list"]).ok();
    assert_eq!(stored["checks"], json!([]));
    assert_eq!(stored["vouched"], false);
    assert!(stored.get("file").is_none(), "{stored}");

    let dir = lab.beside("empty");
    fs::create_dir_all(&dir).unwrap();
    let listed = list_dir(&lab, &dir).ok();
    assert_eq!(listed, json!({"vouched": true, "checks": []}));
}

/// `vouched` is true in every answer of the directory form, a refusal's details included, and
/// false in every answer of the store form.
#[test]
fn the_directory_form_says_the_caller_vouched() {
    let lab = Lab::new();
    let dir = lab.beside("tree");
    write(&dir, ".swarmops/checks.toml", FIXTURE);
    assert_eq!(list_dir(&lab, &dir).ok()["vouched"], true);
    write(&dir, ".swarmops/checks.toml", REFUSED);
    let refused = list_dir(&lab, &dir).err("E_CHECK_FILE", 1);
    assert_eq!(refused["details"]["vouched"], true, "{refused}");
    assert_eq!(refused["details"]["line"], 11, "{refused}");

    lab.publish(".swarmops/checks.toml", FIXTURE);
    assert_eq!(lab.so(&["check", "list"]).ok()["vouched"], false);
    lab.publish(".swarmops/checks.toml", REFUSED);
    let refused = lab.so(&["check", "list"]).err("E_CHECK_FILE", 1);
    assert_eq!(refused["details"]["vouched"], false, "{refused}");
}

/// The directory form opens no store: a store named that does not exist changes nothing, and
/// the envelope names none. Both forms at once is a usage error.
#[test]
fn the_directory_form_opens_no_store() {
    let lab = Lab::new();
    let dir = lab.beside("tree");
    write(&dir, ".swarmops/checks.toml", FIXTURE);
    let line = [
        "--repo",
        "/nonexistent/store",
        "--as",
        "x",
        "check",
        "list",
        "--dir",
    ];
    let named = run_in(
        lab.store.tmp(),
        &[("SWARMOPS_REPO", "/nonexistent/other")],
        &[&line[..], &[dir.to_str().unwrap()]].concat(),
        None,
    );
    assert_eq!(named.ok()["checks"].as_array().unwrap().len(), 3);
    assert!(named.envelope()["meta"].get("repo").is_none(), "{named}");

    let both = lab.so(&["check", "list", "main", "--dir", "."]);
    let error = both.err("E_INVALID_ARG", 2);
    assert!(
        error["message"].as_str().unwrap().contains("--dir"),
        "{error}"
    );
    let nowhere = list_dir(&lab, &lab.beside("missing"));
    assert_eq!(nowhere.err("E_INVALID_ARG", 2)["details"]["reason"], "dir");
}

/// A symbolic link at `.swarmops/checks.toml` is refused, `not_a_file`, in the tree and in a
/// directory: its target is never read.
#[test]
fn a_link_is_not_a_check_file() {
    let lab = Lab::new();
    publish_changes(
        &lab,
        &json!([
            {"path": "other.toml", "op": "put", "content": {"inline": FIXTURE}},
            {"path": ".swarmops/checks.toml", "op": "put", "mode": "symlink",
             "content": {"inline": "../other.toml"}},
        ]),
    );
    let stored = lab.so(&["check", "list"]).err("E_CHECK_FILE", 1);
    assert_eq!(stored["details"]["reason"], "not_a_file", "{stored}");

    let dir = lab.beside("tree");
    write(&dir, "other.toml", FIXTURE);
    fs::create_dir_all(dir.join(".swarmops")).unwrap();
    std::os::unix::fs::symlink("../other.toml", dir.join(".swarmops/checks.toml")).unwrap();
    let linked = list_dir(&lab, &dir).err("E_CHECK_FILE", 1);
    assert_eq!(linked["details"]["reason"], "not_a_file", "{linked}");
    assert_eq!(linked["details"]["vouched"], true, "{linked}");
}

/// A `workspaces.json` to publish, made from where its check is to leave its mark.
type Config = fn(&Path) -> String;

/// `check.argv` of `workspaces.json` still reads as the one check `default`, and `check list`
/// refuses every file `ws land --check` refuses: where it lists `default`, the land runs that
/// argv; where it lists none, the land has no command to run; where the land refuses the file, so
/// does `check list`. The one pinned disagreement: `check list` holds `check.timeout_s` (and
/// `check.argv`) to version 1's rules, which the land does not read, so a timeout past 86,400 s
/// is refused by `check list` and run by the land (known limitations, target M31-10). Each case
/// lands from a workspace made before the file was published.
#[test]
fn the_legacy_check_is_what_a_checked_land_runs() {
    let ran = |lab: &Lab| lab.beside("ran.txt");
    let cases: [(&str, Config); 6] = [
        ("valid", |ran| {
            let script = format!("echo checked > {}", ran.display());
            json!({"check": {"argv": ["sh", "-c", script]}}).to_string()
        }),
        ("empty", |_| r#"{"check": {"argv": []}}"#.to_owned()),
        ("no check", |_| r#"{"prefixes": []}"#.to_owned()),
        ("not json", |_| "{\"check\": ".to_owned()),
        ("argv a string", |_| {
            r#"{"check": {"argv": "make"}}"#.to_owned()
        }),
        ("timeout past version 1", |ran| {
            let script = format!("echo checked > {}", ran.display());
            json!({"check": {"argv": ["sh", "-c", script], "timeout_s": 100_000}}).to_string()
        }),
    ];
    for (case, config) in cases {
        // 1. A workspace with an edit, then the file at main.
        let lab = Lab::new();
        let dir = lab.workspace("w1");
        write(&dir, "src/new.rs", "pub fn g() {}\n");
        let config = config(&ran(&lab));
        lab.publish(".swarmops/workspaces.json", &config);

        // 2. What each says.
        let listed = lab.so(&["check", "list"]);
        let landed = inside(&dir, &["ws", "land", "-m", "land", "--check"]);
        match case {
            "valid" => {
                let checks = listed.ok()["checks"].clone();
                let argv = &checks[0]["argv"];
                assert_eq!(checks[0]["name"], "default", "{case}");
                let given: Value = serde_json::from_str(&config).unwrap();
                assert_eq!(argv, &given["check"]["argv"], "{case}");
                assert_eq!(landed.exit, 0, "{case}: {landed}");
                let said = fs::read_to_string(ran(&lab)).expect("the check ran");
                assert_eq!(said, "checked\n", "{case}");
            }
            "timeout past version 1" => {
                let refused = listed.err("E_CHECK_FILE", 1);
                assert_eq!(refused["details"]["key"], "check.timeout_s", "{refused}");
                assert_eq!(landed.exit, 0, "{case}: {landed}");
                let said = fs::read_to_string(ran(&lab)).expect("the check ran");
                assert_eq!(said, "checked\n", "{case}");
            }
            "empty" | "no check" => {
                assert_eq!(listed.ok()["checks"], json!([]), "{case}");
                let error = landed.err("E_INVALID_ARG", 2);
                assert_eq!(error["details"]["missing"], "check", "{case}: {error}");
            }
            _ => {
                let refused = listed.err("E_CHECK_FILE", 1);
                assert_eq!(refused["details"]["file"], ".swarmops/workspaces.json");
                let error = landed.err("E_INVALID_ARG", 2);
                assert!(
                    error["message"]
                        .as_str()
                        .unwrap()
                        .contains("workspaces.json"),
                    "{case}: {error}"
                );
            }
        }
        assert!(
            !ran(&lab).exists() || case == "valid" || case == "timeout past version 1",
            "{case}: nothing ran"
        );
    }
}

/// This repository's own `.swarmops/checks.toml` lists through the directory form, as `just check`
/// lists it: the gate's six checks, each a recipe of the justfile run by `just`, with no shell.
#[test]
fn this_repository_s_checks_are_its_recipes() {
    // 1. The file, listed from the repository's root.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let lab = Lab::new();
    let listed = list_dir(&lab, &root).ok();
    assert_eq!(listed["vouched"], true, "{listed}");
    assert_eq!(listed["file"], ".swarmops/checks.toml", "{listed}");
    let checks = listed["checks"].as_array().unwrap();
    let names: Vec<&str> = checks.iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "check",
            "check-compare",
            "lint-workflows",
            "server-acceptance",
            "server-entrances",
            "site-check",
        ]
    );

    // 2. Each one's stages: the gate's two before-land, the rest manual (M30-6).
    let stages: Vec<(&str, &Value)> = (checks.iter())
        .map(|c| (c["name"].as_str().unwrap(), &c["stages"]))
        .collect();
    let (gate, manual) = (json!(["before-land"]), json!(["manual"]));
    assert_eq!(
        stages,
        [
            ("check", &gate),
            ("check-compare", &gate),
            ("lint-workflows", &manual),
            ("server-acceptance", &manual),
            ("server-entrances", &manual),
            ("site-check", &manual),
        ]
    );
    let text = fs::read_to_string(root.join(".swarmops/checks.toml")).unwrap();
    assert_eq!(
        text.matches("\nstages = ").count(),
        6,
        "every check writes its stages"
    );

    // 3. Each one `just <recipe>`, at the root, the recipe its name, and the justfile has it.
    let justfile = fs::read_to_string(root.join("justfile")).unwrap();
    let recipes: Vec<&str> = (justfile.lines())
        .filter(|line| !line.starts_with([' ', '\t', '#']) && !line.contains(":="))
        .filter_map(|line| line.split([' ', ':']).next())
        .filter(|name| !name.is_empty() && *name != "set")
        .collect();
    for check in checks {
        let name = check["name"].as_str().unwrap();
        assert_eq!(check["argv"], json!(["just", name]), "{name}");
        assert_eq!(check["dir"], ".", "{name}");
        assert!(recipes.contains(&name), "{name}: no recipe in {recipes:?}");
    }
}

/// The four recipes that stand for a workflow job of several steps run the job's own commands, so
/// that `.swarmops/checks.toml` names what GitHub runs: a change to one side alone fails here.
/// The differences are the ones the justfile's comment names, and nothing else: a temporary
/// directory in place of `RUNNER_TEMP`, the actionlint pins as shell variables in place of the
/// step's `env`, and the site job's branch for the build and its stamp check against the commit
/// built, which only the job knows.
#[test]
fn the_check_recipes_run_their_jobs_steps() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let read = |path: &str| fs::read_to_string(root.join(path)).unwrap();
    let justfile = read("justfile");
    let without = |commands: Vec<String>, dropped: &[&str]| -> Vec<String> {
        (commands.into_iter())
            .filter(|command| !dropped.contains(&command.as_str()))
            .collect()
    };

    // 1. server.yml's two jobs, line for line after the toolchain's install.
    let server = read(".github/workflows/server.yml");
    for (recipe, job) in [
        ("server-entrances", "entrances"),
        ("server-acceptance", "acceptance"),
    ] {
        let (deps, commands) = recipe_commands(&justfile, recipe);
        assert!(deps.is_empty(), "{recipe}: {deps:?}");
        let steps = without(
            job_commands(&server, job),
            &["rustup toolchain install --no-self-update"],
        );
        assert_eq!(commands, steps, "{recipe} and server.yml's {job}");
    }

    // 2. site.yml's `site` job: the recipe's dependencies run as `just <dep>`, the build's branch
    //    and the stamp check against the commit built aside.
    let site = read(".github/workflows/site.yml");
    let (deps, commands) = recipe_commands(&justfile, "site-check");
    let mut recipe: Vec<String> = deps.iter().map(|dep| format!("just {dep}")).collect();
    recipe.extend(commands);
    let steps: Vec<String> = (job_commands(&site, "site").into_iter())
        .filter(|command| !command.starts_with("node web/scripts/check-stamp.mjs --sha "))
        .map(|command| command.replace("GITHUB_REF_NAME=\"$SCOPE_BRANCH\" ", ""))
        .collect();
    assert_eq!(recipe, steps, "site-check and site.yml's site");

    // 3. ci.yml's `zizmor` job after its zizmor audit, the pins and the directory aside.
    let ci = read(".github/workflows/ci.yml");
    let (deps, commands) = recipe_commands(&justfile, "lint-workflows");
    assert!(deps.is_empty(), "lint-workflows: {deps:?}");
    let job = job_text(&ci, "zizmor");
    let pin = |key: &str| {
        let line = (job.lines())
            .find_map(|line| line.trim().strip_prefix(&format!("{key}: ")))
            .unwrap_or_else(|| panic!("ci.yml's zizmor job sets no {key}"));
        line.trim().to_owned()
    };
    let version = format!("version={}", pin("ACTIONLINT_VERSION"));
    let sha256 = format!("sha256={}", pin("ACTIONLINT_SHA256"));
    assert!(commands.contains(&version), "{version}: {commands:#?}");
    assert!(commands.contains(&sha256), "{sha256}: {commands:#?}");
    let recipe = without(
        commands,
        &[
            "#!/usr/bin/env bash",
            "set -euo pipefail",
            &version,
            &sha256,
            "dir=\"$(mktemp -d)\"",
            "trap 'rm -rf \"$dir\"' EXIT",
        ],
    );
    let steps: Vec<String> = without(
        job_commands(&ci, "zizmor"),
        &["dir=\"$RUNNER_TEMP/actionlint\"", "mkdir -p \"$dir\""],
    )
    .into_iter()
    .map(|command| {
        (command.replace("ACTIONLINT_VERSION", "version")).replace("ACTIONLINT_SHA256", "sha256")
    })
    .collect();
    assert_eq!(recipe, steps, "lint-workflows and ci.yml's zizmor");
}

/// The text of `job` in the workflow `text`: its lines, from `  <job>:` to the next job.
fn job_text(text: &str, job: &str) -> String {
    let head = format!("  {job}:");
    let mut lines = text.lines().skip_while(|line| *line != head);
    assert!(lines.next().is_some(), "no job {job}");
    lines
        .take_while(|line| line.is_empty() || line.starts_with("   ") || line.starts_with("  #"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The commands `job`'s `run:` steps hold, in order: a block's lines, comments and blank lines
/// left out, a line ending in `\` joined to the next.
fn job_commands(text: &str, job: &str) -> Vec<String> {
    let job = job_text(text, job);
    let lines: Vec<&str> = job.lines().collect();
    let mut script = Vec::new();
    let mut at = 0;
    while at < lines.len() {
        let line = lines[at];
        let trimmed = line.trim_start();
        let key = trimmed.strip_prefix("- ").unwrap_or(trimmed);
        at += 1;
        let Some(value) = key.strip_prefix("run:") else {
            continue;
        };
        let value = value.trim();
        if value != "|" {
            script.push(value.to_owned());
            continue;
        }
        let indent = line.len() - trimmed.len();
        while at < lines.len() {
            let next = lines[at];
            let deeper = next.len() - next.trim_start().len() > indent;
            if !next.trim().is_empty() && !deeper {
                break;
            }
            script.push(next.trim().to_owned());
            at += 1;
        }
    }
    joined(script)
}

/// The dependencies of `recipe` and the commands of its body: its lines, comments and blank lines
/// left out, a line ending in `\` joined to the next.
fn recipe_commands(justfile: &str, recipe: &str) -> (Vec<String>, Vec<String>) {
    let mut lines = justfile
        .lines()
        .skip_while(|line| line.split(':').next() != Some(recipe) || line.contains(":="));
    let head = lines.next().unwrap_or_else(|| panic!("no recipe {recipe}"));
    let deps = head[recipe.len() + 1..]
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    let body = lines
        .take_while(|line| line.starts_with("    "))
        .map(|line| line.trim().to_owned())
        .collect();
    (deps, joined(body))
}

/// `lines` as commands: comments and blank lines left out, a line ending in `\` joined to the
/// next, each trimmed.
fn joined(lines: Vec<String>) -> Vec<String> {
    let mut commands: Vec<String> = Vec::new();
    let mut open = false;
    for line in lines {
        if !open && (line.is_empty() || (line.starts_with('#') && !line.starts_with("#!"))) {
            continue;
        }
        let (text, continued) = match line.strip_suffix('\\') {
            Some(text) => (text.trim_end().to_owned(), true),
            None => (line, false),
        };
        match commands.last_mut() {
            Some(last) if open => {
                last.push(' ');
                last.push_str(&text);
            }
            _ => commands.push(text),
        }
        open = continued;
    }
    commands
}
