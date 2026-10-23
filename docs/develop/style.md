# Code style

SwarmOps follows the [Google Rust style guide](https://google.github.io/styleguide/rustguide.html),
on top of the official Rust style (`rustfmt`) and the Rust API guidelines. This page says how each
of its rules is kept here: which ones a tool enforces, which ones a reviewer reads for, and the
few the project sets aside, with the reason. Apply the guide as written wherever this page is
silent.

- [Enforced by the gate](#enforced-by-the-gate)
- [Read for in review](#read-for-in-review)
- [Constructing public types](#constructing-public-types)
- [Deviations](#deviations)

## Enforced by the gate

`just check` ([CONTRIBUTING](../../CONTRIBUTING.md#the-definition-of-done-just-check)) fails on
these; nothing here needs a reviewer.

| Rule of the guide | How it is kept |
| --- | --- |
| Use the formatting tool | `cargo fmt --check`, default `max_width = 100`. The guide's other options (`imports_granularity`, `hex_literal_case`, `format_macro_matchers`) need nightly `rustfmt`; the first two hold by hand (see below), the third does not apply. |
| One module per `use`; no `::` inside braces | Kept by hand; none in the tree. |
| No `unsafe` | `unsafe_code = "forbid"` in the workspace `Cargo.toml`. |
| Public items documented | `missing_docs`, denied under `-D warnings`. |
| `expect` over `unwrap` | `clippy::unwrap_used`, denied for library and binary code (the second clippy step of `just check`). Tests, benches and `swarmops-testkit` may unwrap: the guide lets test code panic on an assertion. |
| Context strings in `assert!` | `clippy::missing_assert_message`, denied for library and binary code, the same way. |
| No APIs in non-leaf modules; files named after their item | `clippy::mod_module_files`, denied for library and binary code: a module with children is `foo.rs` beside `foo/`, and holds only `mod` lines and re-exports. (`tests/support/mod.rs` stays: cargo would build a `tests/support.rs` as a test of its own.) |
| Flexible references (`&[T]` not `&Vec<T>`, `&str` not `&String`) | `clippy::ptr_arg`, `clippy::borrowed_box` (in `clippy::all`, denied); `clippy::rc_buffer` for `Arc<Vec<T>>`. |
| Standard traits over custom methods; `Default` beside `new()`; `is_empty()` beside `len()`; `as_`/`to_`/`into_` conventions | `clippy::should_implement_trait`, `new_without_default`, `len_without_is_empty`, `wrong_self_convention` (`clippy::all`). |
| `#[non_exhaustive]` on public enums and structs | `clippy::exhaustive_enums` and `clippy::exhaustive_structs`, denied in the workspace lint table. Other crates build these types with the defining crate's `new` or `Default` ([below](#constructing-public-types)) and match them with a wildcard that fails closed (`RepoError::Internal`, `StoreError::Internal`, `E_INTERNAL`). `swarmops-testkit` is exempt: test support. |
| Enums over bools; too many bools | `clippy::fn_params_excessive_bools` (`clippy::pedantic`, warns, fails under `-D warnings`), and review for the rest. |
| No glob imports | `clippy::wildcard_imports` (`pedantic`). It allows a third-party prelude (`rayon::prelude::*`, `proptest::prelude::*`) and `use super::*` in an inline test module, as the guide does. |
| No unstable features | The toolchain is stable (`rust-toolchain.toml`). |
| Supply chain and licenses | `cargo deny`, the pinned workflows (`SECURITY.md`). |

## Read for in review

The lenses in `.claude/agents/` and every human reviewer read for these; the guide's text is the
reference.

- **Qualify free functions with their module, import types and traits by name.**
  `bytes::to_u64(x)`, not `use swarmops_core::bytes::to_u64;`. Exceptions as in the guide:
  `std::fs`-style module imports are the norm, and a macro is imported by name.
- **Hex literals in lower case** (`0x7f`), since stable `rustfmt` cannot enforce it.
- **Enum, not `bool`, for a parameter whose meaning the call site does not show.** A `bool` that
  stays gets a comment at the call (`yes_no(value)` needs none).
- **Newtypes over aliases** where the semantics differ; **named fields over tuples**, a one-field
  newtype excepted.
- **A trait only for generic programming, `impl Trait`/`dyn Trait` or an extension** (`…Ext`,
  methods only). Never to group code.
- **Borrow rather than clone; `Rc`/`Arc` only where ownership cannot be unique**, decided in
  review. The store's shared handles (`FsOps`, the object store, the derived-value engine) are
  the agreed cases.
- **Panics**: `panic!` marks a bug, never a runtime condition; a function that may panic on its
  input documents it (`# Panics`).
- **`#[must_use]`** on a function whose result it is wrong to drop, when the type is not already
  `#[must_use]`.
- **Type annotations** for `.into()`, `.parse()`, `collect()` and long iterator results; a
  variable's type before a turbofish.
- **Short combinator chains**; a long one is bound to a name first, never built in a `for` header.
- **Doc comments**: `///` and `//!`, Markdown, a sentence that adds information or nothing.
- **Readability**: a blank line between logical blocks, a short comment per block that says what
  and why, one call per line in a method chain, in the hunks a change touches.
- **Public surfaces are designed for extension.** "One team owns every caller" is never a design
  reason: not for an exhaustive type, a missing constructor, or a `match` without a fail-closed
  wildcard.

## Constructing public types

Outside the defining crate a struct literal does not compile, nor does the update syntax
`Req { a, ..Default::default() }`; callers use the type's constructor. A field is required when it
is not an `Option` and has no `serde(default)`; in a type without serde, every field is. A type
with no required field derives `Default`, and the caller assigns the rest
(`let mut r = Req::default(); r.deep = true;`); any other type has `new(required…)`, its other
fields set to their serde defaults. A type with an invariant has no `Default`: its `new` takes
every field and runs the existing check, and the invariant is checked again where it fails closed.

## Deviations

Each is a decision of the maintainers, recorded here; a change that lifts one removes its line.

| Rule of the guide | Decision | Why |
| --- | --- | --- |
| No `panic::catch_unwind` in first-party code | Kept in the CLI entry and the MCP server (`crates/swarmops-cli/src/lib.rs`, `mcp.rs`). | Defense in depth, as the guide's exception allows: a panic is still a bug and is reported as `E_INTERNAL`; the server keeps answering other requests instead of dropping the agent's session. Nothing relies on it for correctness, and the binaries build with `panic = "unwind"`. |
| Descriptive lifetime names | Single letters stay. | The guide allows up to two local single-letter lifetimes; nearly every lifetime here is one, in a signature that relates an input to an output. Renaming them would change thousands of lines for no reader. |
| Prefer small crates, one file per crate | Ten crates along the layer boundaries of the design. | The crates follow the architecture ([architecture](../architecture.md)); a crate per file would spread the hash contract over dozens of crates. |
| Return iterators instead of `Vec<T>` | Store-facing APIs return owned, serializable types. | Every operation takes and returns serde types, the future RPC seam (`AGENTS.md`); an iterator cannot cross it. |
