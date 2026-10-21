//! The checks for a name that every platform can hold.
//!
//! Windows cannot store some names as file names: a device name (`con`, `nul`, `com1`, with or
//! without an extension), a name that ends in a dot or a space, and a name with a character it
//! forbids. The rules are those of that platform, applied on every one, so that a command
//! behaves alike on all three and a store's refs and an export's trees can be files wherever
//! they go (storage-core design, §2.1 P8). Nothing here asks the host what it can hold, and this
//! crate performs no I/O.
