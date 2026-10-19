//! The workspace's lint policy stays in force. Nothing else notices if it goes: a crate whose
//! manifest drops `[lints] workspace = true` is no longer linted by the table, and a table
//! without `missing_docs` stops the documentation gate, while `just check` stays green. These
//! tests read the manifests as text (no TOML parser, no new dependency) and fail instead.
//! `fuzz/` and `bench/compare/` are workspaces of their own, with lint tables of their own: they
//! cannot inherit this one, and are not read here.

use std::fs;
use std::path::{Path, PathBuf};

/// The workspace root: this crate is `crates/swarmops-core`.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A manifest line without its comment and spaces: `workspace = true # why` is `workspace=true`.
fn squeeze(line: &str) -> String {
    let code = line.split_once('#').map_or(line, |(code, _)| code);
    code.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Whether `[table]` of the manifest holds the line `entry`, comments and spaces aside. Another
/// table's line does not count: `missing_docs = "warn"` under `[workspace.lints.clippy]` lints
/// nothing.
fn table_holds(manifest: &str, table: &str, entry: &str) -> bool {
    let (header, entry) = (format!("[{table}]"), squeeze(entry));
    let mut inside = false;
    for line in manifest.lines().map(squeeze) {
        if line.starts_with('[') {
            inside = line == header;
        } else if inside && line == entry {
            return true;
        }
    }
    false
}

/// The root manifest makes `missing_docs` a warning, which `just check` denies in clippy and in
/// rustdoc (`-D warnings`), while a local build of a draft still runs.
fn keeps_missing_docs(root_manifest: &str) -> bool {
    table_holds(
        root_manifest,
        "workspace.lints.rust",
        r#"missing_docs = "warn""#,
    )
}

/// A crate's manifest takes the workspace's lints.
fn inherits_workspace_lints(crate_manifest: &str) -> bool {
    table_holds(crate_manifest, "lints", "workspace = true")
}

#[test]
fn the_workspace_table_keeps_missing_docs_a_warning() {
    let root = fs::read_to_string(workspace_root().join("Cargo.toml")).unwrap();
    assert!(
        keeps_missing_docs(&root),
        "Cargo.toml: [workspace.lints.rust] must hold `missing_docs = \"warn\"`"
    );
}

#[test]
fn every_crate_inherits_the_workspace_lints() {
    let mut crates = 0;
    let mut without = Vec::new();
    for entry in fs::read_dir(workspace_root().join("crates")).unwrap() {
        let dir = entry.unwrap().path();
        if !dir.join("Cargo.toml").is_file() {
            continue;
        }
        crates += 1;
        let manifest = fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        if !inherits_workspace_lints(&manifest) {
            without.push(dir.file_name().unwrap().to_string_lossy().into_owned());
        }
    }
    assert!(crates > 0, "no crate found under crates/");
    assert!(
        without.is_empty(),
        "these crates lack `[lints] workspace = true`: {without:?}"
    );
}

/// The two checks above read the table they name: a manifest that lacks what they look for
/// fails them, however nearly it is spelt, and one that has it passes.
#[test]
fn the_checks_read_the_table_they_name() {
    for root in [
        "",
        "[workspace.lints.rust]\nunsafe_code = \"forbid\"\n",
        "[workspace.lints.rust]\nmissing_docs = \"allow\"\n",
        "[workspace.lints.rust]\n# missing_docs = \"warn\"\n",
        "[workspace.lints.clippy]\nmissing_docs = \"warn\"\n",
        "[lints.rust]\nmissing_docs = \"warn\"\n",
    ] {
        assert!(!keeps_missing_docs(root), "{root:?}");
    }
    for krate in [
        "[package]\nname = \"x\"\n",
        "[lints]\n",
        "[lints]\nworkspace = false\n",
        "[lints]\n# workspace = true\n",
        "[package]\nworkspace = true\n\n[lints]\n",
        "[lints]\n\n[dependencies]\nworkspace = true\n",
    ] {
        assert!(!inherits_workspace_lints(krate), "{krate:?}");
    }
    let root = "[workspace]\nmembers = []\n\n[workspace.lints.rust]\nunsafe_code = \"forbid\"\n\
                missing_docs=\"warn\"  # a warning\n\n[workspace.lints.clippy]\nall = \"deny\"\n";
    assert!(keeps_missing_docs(root));
    let krate = "[package]\nname = \"x\"\n\n[lints]\n# the workspace's\nworkspace = true\n\n\
                 [[test]]\nname = \"all\"\n";
    assert!(inherits_workspace_lints(krate));
}
