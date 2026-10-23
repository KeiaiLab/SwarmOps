//! The portability rules of a materialisation (§3.3 of the M5 design): case collisions on a
//! case-insensitive volume, names Windows refuses, and the reserved top-level `.swarmws`, all
//! checked before anything is written. A refusal is `E_WS_UNPORTABLE{reason, paths}`, and
//! nothing is written.
//!
//! The rules over names are M12's (`swarmops_core::portable`); this module applies them to the
//! set of paths a materialisation will hold.

use std::collections::{BTreeSet, HashMap};

use serde_json::{Map, Value};
use swarmops_api::{ErrorCode, OpError};

use crate::state::SWARMWS_DIR;

/// The most case-collision pairs a refusal names.
const MAX_PAIRS: usize = 5;

/// The most paths a refusal names.
const MAX_PATHS: usize = 20;

/// Whose names a materialisation must hold: Windows refuses more of them than the others do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Platform {
    /// Windows: its device names, `\ : * ? " < > |`, control characters, a trailing dot or
    /// space are refused.
    Windows,
    /// Any other: every UTF-8 name is held.
    Other,
}

/// The platform this build runs on.
pub(crate) const HOST: Platform = if cfg!(windows) {
    Platform::Windows
} else {
    Platform::Other
};

/// Refuses the paths a materialisation adds, `added`, unless the volume can hold them:
///
/// 1. a top-level `.swarmws`, on every platform (`reserved_name`);
/// 2. on Windows, a component `check_component` refuses (`windows_name`);
/// 3. with `case_set` (a case-insensitive volume: every path the workspace will hold that can
///    meet an added one, `added` among them), two that differ only in case (`case_collision`,
///    the pairs).
pub(crate) fn check(
    added: &[String],
    case_set: Option<&[String]>,
    platform: Platform,
) -> Result<(), OpError> {
    // 1. The workspace's own directory, at the top.
    let reserved: Vec<&String> = (added.iter())
        .filter(|path| path.split('/').next() == Some(SWARMWS_DIR))
        .collect();
    if !reserved.is_empty() {
        return Err(refused("reserved_name", paths_value(&reserved)));
    }

    // 2. Names Windows cannot hold.
    if platform == Platform::Windows {
        let refused_names: Vec<&String> = (added.iter())
            .filter(|path| {
                path.split('/')
                    .any(|c| swarmops_core::portable::check_component(c).is_err())
            })
            .collect();
        if !refused_names.is_empty() {
            return Err(refused("windows_name", paths_value(&refused_names)));
        }
    }

    // 3. Paths one name apart in case, where the volume folds case.
    let pairs = case_set.map(collisions).unwrap_or_default();
    if pairs.is_empty() {
        return Ok(());
    }
    let pairs: Vec<Value> = (pairs.into_iter())
        .map(|[a, b]| Value::from(vec![a, b]))
        .collect();
    Err(refused("case_collision", Value::from(pairs)))
}

/// The pairs of `paths` that name one file on a case-insensitive volume: the first two
/// spellings of each path or directory that differ only in ASCII case, per component (as M12
/// compares refs), each pair in byte order; the first [`MAX_PAIRS`] of them in byte order.
pub(crate) fn collisions(paths: &[String]) -> Vec<[String; 2]> {
    // 1. Every path and every directory above it, by its folded spelling.
    let mut sorted: Vec<&String> = paths.iter().collect();
    sorted.sort();
    sorted.dedup();
    let mut seen: HashMap<String, String> = HashMap::new();
    let mut pairs = BTreeSet::new();
    for path in sorted {
        let mut end = 0;
        for component in path.split('/') {
            // 2. This prefix of the path, as spelled and as folded.
            end += component.len();
            let spelled = &path[..end];
            end += 1;
            let folded = spelled.to_ascii_lowercase();

            // 3. Another spelling of it seen before: a pair, and the deeper ones would repeat it.
            match seen.get(&folded) {
                Some(first) if first != spelled => {
                    pairs.insert([first.clone(), spelled.to_owned()]);
                    break;
                }
                Some(_) => {}
                None => {
                    seen.insert(folded, spelled.to_owned());
                }
            }
        }
    }
    pairs.into_iter().take(MAX_PAIRS).collect()
}

/// The first [`MAX_PATHS`] of `paths`, as a refusal names them.
fn paths_value(paths: &[&String]) -> Value {
    Value::from(
        (paths.iter())
            .take(MAX_PATHS)
            .map(|path| Value::from(path.as_str()))
            .collect::<Vec<_>>(),
    )
}

/// `E_WS_UNPORTABLE` for `reason`, naming `paths`.
fn refused(reason: &str, paths: Value) -> OpError {
    OpError::new(
        ErrorCode::WsUnportable,
        format!("the tree holds paths this workspace's volume cannot hold ({reason}): {paths}"),
    )
    .with_details(Map::from_iter([
        ("reason".to_owned(), Value::from(reason)),
        ("paths".to_owned(), paths),
    ]))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The owned paths of `paths`.
    fn owned(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| (*p).to_owned()).collect()
    }

    /// Pairs are found per component, a directory's once for all the files below it, and are
    /// given in byte order; paths that differ in more than case are no pair.
    #[test]
    fn collisions_pair_spellings_of_one_path() {
        let paths = owned(&[
            "src/readme",
            "src/README",
            "Lib/a",
            "lib/b",
            "lib/c",
            "ok/x",
            "ok/y",
            "é/a",
            "É/a",
        ]);
        assert_eq!(
            collisions(&paths),
            [
                ["Lib".to_owned(), "lib".to_owned()],
                ["src/README".to_owned(), "src/readme".to_owned()],
            ]
        );
        assert_eq!(
            collisions(&owned(&["a", "b/a", "a/b"])),
            Vec::<[String; 2]>::new()
        );
    }

    /// Each rule refuses with its reason and names the paths; Windows names only on Windows;
    /// the case rule only with a case set; `.swarmws` only at the top.
    #[test]
    fn check_refuses_with_the_reason_and_the_paths() {
        // 1. The reserved name, at the top only.
        let reserved = check(&owned(&[".swarmws/x", "a"]), None, Platform::Other).unwrap_err();
        assert_eq!(reserved.code, "E_WS_UNPORTABLE");
        assert_eq!(reserved.details["reason"], "reserved_name");
        assert_eq!(reserved.details["paths"], json!([".swarmws/x"]));
        check(&owned(&["a/.swarmws/x"]), None, Platform::Other).unwrap();

        // 2. Windows names, on Windows.
        let names = owned(&["src/aux.txt", "src/a:b", "src/ok", "src/dot."]);
        check(&names, None, Platform::Other).unwrap();
        let windows = check(&names, None, Platform::Windows).unwrap_err();
        assert_eq!(windows.details["reason"], "windows_name");
        assert_eq!(
            windows.details["paths"],
            json!(["src/aux.txt", "src/a:b", "src/dot."])
        );

        // 3. Case collisions, with a case set.
        let added = owned(&["src/README"]);
        let set = owned(&["src/README", "src/readme"]);
        check(&added, None, Platform::Other).unwrap();
        let case = check(&added, Some(&set), Platform::Other).unwrap_err();
        assert_eq!(case.details["reason"], "case_collision");
        assert_eq!(case.details["paths"], json!([["src/README", "src/readme"]]));
    }
}
