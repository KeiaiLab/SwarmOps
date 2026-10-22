//! `index.bin` (§4.2 of the M5 design): the workspace's cache of one row per materialised file
//! (size, mtime, inode, mode, blob), columnar, checksummed, written without a sync.
//!
//! Empty until the M5 plan's task W6.
