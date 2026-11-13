//! `serve`'s two request parsers never panic and never take what is not one store's name (M14
//! design §2.1, §2.2). The first input byte picks the parser, the rest is its input as lossy
//! UTF-8:
//!
//! ```text
//! even ─▶ parse_ssh(SSH_ORIGINAL_COMMAND)     odd ─▶ cgi::fuzz_parse_path(PATH_INFO)
//! accepted: one remote segment, no `/`, not `.` or `..`, and the name read back the same
//!           (ssh: git-upload-pack <name quoted as git quotes it>; CGI: /<name>.git/<request>)
//! refused:  E_INVALID_ARG or E_NOT_A_REPO, a known reason, a text no input could change
//! ```
#![no_main]

use libfuzzer_sys::fuzz_target;
use swarmops_api::OpError;
use swarmops_ops::serve::cgi::fuzz_parse_path;
use swarmops_ops::serve::entry::parse_ssh;

/// The reasons an ssh command is refused for.
const SSH_REASONS: [&str; 3] = ["not_served", "unknown_command", "bad_path"];

/// The reasons a CGI path is refused for.
const CGI_REASONS: [&str; 2] = ["bad_path", "not_smart_http"];

/// Inputs that give each of the parsers' refusals, its texts the only ones any input may get.
const SSH_REFUSED: [&str; 4] = [
    "swarmops serve --stdio",
    "",
    "git-upload-pack acme",
    "git-upload-pack ''",
];
const CGI_REFUSED: [&str; 2] = ["/Acme/info/refs", ""];

/// A refusal's code, message and hint.
type Text = (String, String, Option<String>);

fuzz_target!(|data: &[u8]| {
    let Some((&parser, rest)) = data.split_first() else {
        return;
    };
    let input = String::from_utf8_lossy(rest);
    if parser % 2 == 0 {
        ssh(&input);
        return;
    }
    cgi(&input);
});

/// The ssh command `command`: a store's name that reads back from git's quoting, or a refusal.
fn ssh(command: &str) {
    // 1. A refusal: a known reason, a known text.
    let request = match parse_ssh(command) {
        Ok(request) => request,
        Err(error) => {
            refused(
                &error,
                &SSH_REASONS,
                &texts(&SSH_REFUSED, |c| parse_ssh(c).err()),
            );
            return;
        }
    };

    // 2. A name, which git's quoting of it gives back.
    one_segment(&request.store);
    let line = format!("git-upload-pack {}", sq_quote(&request.store));
    let again = parse_ssh(&line).unwrap_or_else(|e| panic!("{line:?}: {e:?}"));
    assert_eq!(again.store, request.store, "{line:?}");
}

/// The CGI path `path`: a store's name and smart request that read back, or a refusal.
fn cgi(path: &str) {
    // 1. A refusal: a known reason, a known text.
    let (store, request) = match fuzz_parse_path(path) {
        Ok(parsed) => parsed,
        Err(error) => {
            let known = texts(&CGI_REFUSED, |p| fuzz_parse_path(p).err());
            refused(&error, &CGI_REASONS, &known);
            return;
        }
    };

    // 2. A name and request, which the canonical path gives back.
    one_segment(&store);
    let canonical = format!("/{store}.git/{request}");
    let again = fuzz_parse_path(&canonical).unwrap_or_else(|e| panic!("{canonical:?}: {e:?}"));
    assert_eq!(again, (store, request), "{canonical:?}");
}

/// `store` is one segment of the remote-name grammar.
fn one_segment(store: &str) {
    assert!(
        swarmops_store::check_remote_segment(store).is_ok(),
        "{store:?}"
    );
    assert!(!store.contains('/'), "{store:?}");
    assert!(store != "." && store != "..", "{store:?}");
}

/// `error` gives one of `reasons`, no other detail, and a code, message and hint from `known`.
fn refused(error: &OpError, reasons: &[&str], known: &[Text]) {
    let reason = error.details.get("reason").and_then(|r| r.as_str());
    assert!(reason.is_some_and(|r| reasons.contains(&r)), "{error:?}");
    assert_eq!(error.details.len(), 1, "{error:?}");
    let text = (
        error.code.clone(),
        error.message.clone(),
        error.hint.clone(),
    );
    assert!(known.contains(&text), "{error:?}");
}

/// The code, message and hint of each refusal `parse` gives `inputs`.
fn texts(inputs: &[&str], parse: impl Fn(&str) -> Option<OpError>) -> Vec<Text> {
    inputs
        .iter()
        .filter_map(|input| parse(input))
        .map(|error| (error.code, error.message, error.hint))
        .collect()
}

/// `value` quoted as git's `sq_quote_buf` writes it: `'…'`, with `'` as `'\''` and `!` as
/// `'\!'`.
fn sq_quote(value: &str) -> String {
    let mut quoted = String::from("'");
    for c in value.chars() {
        match c {
            '\'' | '!' => {
                quoted.push_str("'\\");
                quoted.push(c);
                quoted.push('\'');
            }
            _ => quoted.push(c),
        }
    }
    quoted.push('\'');
    quoted
}
