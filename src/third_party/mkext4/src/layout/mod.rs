//! Layout: geometry (where fixed metadata lives) and the deterministic
//! block allocator (where everything else goes).

pub mod alloc;
pub mod geometry;

pub use geometry::Geometry;
