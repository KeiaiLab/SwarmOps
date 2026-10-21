//! Which volumes a store may live on (storage-core design, §2.1 P4).
//!
//! One check per platform answers whether a path is on a volume the store supports: on Linux the
//! path's name, on macOS the type of the volume it is on, and on Windows whether the path is a
//! network share. A path that does not exist yet is judged by the nearest directory above it
//! that does.
