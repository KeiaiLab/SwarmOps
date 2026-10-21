//! Paths in the API: a path inside a store is joined with `/` on every platform, a host path
//! comes back as the caller gave it, and the store root has no verbatim prefix (storage-core
//! design, §2.1 P9).
