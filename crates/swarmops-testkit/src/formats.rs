//! A sweep run once per store format: [`per_format!`](crate::per_format!).
//!
//! ```text
//! per_format! { fn a_sweep(format) { .. } }
//!   mod a_sweep {
//!       fn run(format: StoreFormat) { .. }
//!       #[test] fn format_1() { run(StoreFormat::V1) }
//!       #[test] fn format_2() { run(StoreFormat::V2) }
//!   }
//! ```
//!
//! Each format's run is a test of its own, `a_sweep::format_1` and `a_sweep::format_2`, so the
//! harness runs them at once and a slow sweep's budget holds per format.

use swarmops_store::StoreFormat;

/// The name of the test [`per_format!`](crate::per_format!) makes for `format` in its module:
/// what a test that runs itself again as a child passes to
/// [`test_name!`](crate::test_name!) from its body.
pub fn test_of(format: StoreFormat) -> &'static str {
    match format {
        StoreFormat::V1 => "format_1",
        StoreFormat::V2 => "format_2",
        other => panic!("per_format! makes no test for {other:?}"),
    }
}

/// Expands `fn <name>(<format>) { <body> }`, with the attributes given before `fn` (the doc,
/// `#[ignore = "…"]`), into a module `<name>` holding the body as `run(<format>: StoreFormat)`
/// and a test per format that `init` creates: `format_1` and `format_2`, each with those
/// attributes. The body sees the items of the module the macro expands in.
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
        }
    };
}

#[cfg(test)]
mod tests {
    use swarmops_store::{INIT_FORMATS, StoreFormat};

    /// [`per_format!`] runs a test per format `init` creates: a new one fails here until the
    /// macro runs it too.
    #[test]
    fn per_format_runs_every_format_init_creates() {
        assert_eq!(INIT_FORMATS, [StoreFormat::V1, StoreFormat::V2]);
    }
}
