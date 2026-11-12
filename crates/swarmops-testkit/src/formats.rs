//! A sweep run once per store format: [`per_format!`](crate::per_format!).
//!
//! ```text
//! per_format! { fn a_sweep(format) { .. } }
//!   mod a_sweep {
//!       fn run(format: StoreFormat) { .. }
//!       #[test] fn format_1() { run(StoreFormat::V1) }
//!       #[test] fn format_2() { run(StoreFormat::V2) }
//!       #[test] fn format_3() { run(StoreFormat::V3) }
//!   }
//! ```
//!
//! Each format's run is a test of its own, `a_sweep::format_1` to `a_sweep::format_3`, so the
//! harness runs them at once and a slow sweep's budget holds per format.

use swarmops_store::StoreFormat;

/// The name of the test [`per_format!`](crate::per_format!) makes for `format` in its module:
/// what a test that runs itself again as a child passes to
/// [`test_name!`](crate::test_name!) from its body.
pub fn test_of(format: StoreFormat) -> &'static str {
    match format {
        StoreFormat::V1 => "format_1",
        StoreFormat::V2 => "format_2",
        StoreFormat::V3 => "format_3",
        other => panic!("per_format! makes no test for {other:?}"),
    }
}

/// Expands `fn <name>(<format>) { <body> }`, with the attributes given before `fn` (the doc,
/// `#[ignore = "…"]`), into a module `<name>` holding the body as `run(<format>: StoreFormat)`
/// and a test per format that `init` creates: `format_1`, `format_2` and `format_3`, each with
/// those attributes. The body sees the items of the module the macro expands in.
#[macro_export]
macro_rules! per_format {
    ($(#[$attr:meta])* fn $name:ident($format:ident) $body:block) => {
        mod $name {
            #[allow(unused_imports)]
            use super::*;

            fn run($format: ::swarmops_store::StoreFormat) $body

            $(#[$attr])*
            #[test]
            fn format_1() {
                run(::swarmops_store::StoreFormat::V1);
            }

            $(#[$attr])*
            #[test]
            fn format_2() {
                run(::swarmops_store::StoreFormat::V2);
            }

            $(#[$attr])*
            #[test]
            fn format_3() {
                run(::swarmops_store::StoreFormat::V3);
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use swarmops_store::{DEFAULT_INIT_FORMAT, INIT_FORMATS, StoreFormat};

    use super::test_of;

    /// [`per_format!`] runs a test per format `init` creates: a new one fails here until the
    /// macro runs it too.
    #[test]
    fn per_format_runs_every_format_init_creates() {
        assert_eq!(
            INIT_FORMATS,
            [StoreFormat::V1, StoreFormat::V2, StoreFormat::V3]
        );
        let names: Vec<&str> = INIT_FORMATS.iter().map(|&format| test_of(format)).collect();
        assert_eq!(names, ["format_1", "format_2", "format_3"]);
    }

    /// `init` creates FORMAT 3 when the request names no format (the FORMAT 3 design §0 D3), and
    /// still creates FORMAT 1 and 2 when it names one.
    #[test]
    fn init_formats_list_v3_and_default_to_it() {
        assert_eq!(DEFAULT_INIT_FORMAT, StoreFormat::V3);
        assert!(INIT_FORMATS.contains(&DEFAULT_INIT_FORMAT));
        assert_eq!(
            INIT_FORMATS.last(),
            Some(&StoreFormat::V3),
            "the newest last"
        );
    }
}
