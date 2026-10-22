//! The process runner (§5.4 of the M5 design) of a check and of the bootstrap: argv without a
//! shell, stdin null, a timeout that kills the command's process group on Unix, the last 8 KiB of
//! each stream kept.
//!
//! Empty until the M5 plan's task W4.
