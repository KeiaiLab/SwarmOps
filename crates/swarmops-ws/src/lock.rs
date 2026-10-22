//! The workspace's lock (§2.4 of the M5 design): `.swarmws/lock`, held exclusively by every
//! mutation, waited for at most 5 s, then `E_WS_LOCKED`.
//!
//! Empty until the M5 plan's task W5.
