//! The generated reference pages, `docs/reference/cli.md`, one page per command in
//! `docs/reference/commands/` and `docs/reference/errors.md`: rendered from the command grammar
//! ([`Cli`](crate::cli::Cli)'s clap tree), the golden CLI steps and from the error table
//! ([`ErrorCode`](swarmops_api::ErrorCode) and its schema), so that the pages cannot drift from
//! the code.
//!
//! ```text
//! clap grammar ───┬─▶ cli.md                 the grouped index (cli_page)
//! golden steps ───┼─▶ commands/<name>.md     one page per command (command_page)
//! notes/commands ─┘
//! error table ─────▶ errors.md
//! ```
//!
//! `reference_pages_match` fails when a committed page is not its rendering;
//! `SWARMOPS_REGEN_REFERENCE=1` with the ignored `regen_reference` writes them again:
//!
//! ```text
//! SWARMOPS_REGEN_REFERENCE=1 cargo nextest run -p swarmops-cli --run-ignored ignored-only \
//!     -E 'test(regen_reference)'
//! ```

mod cli_page;
mod command_page;
mod limits;
mod pages;
mod schema_types;
