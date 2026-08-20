//! notespace core domain model.
//!
//! This crate is deliberately free of I/O, of `wasm`/native awareness, and of any
//! dependency that cannot compile to `wasm32-unknown-unknown` (DESIGN.md §9). Everything
//! here is unit-testable without a database or a network.

pub mod model;
pub mod path;
pub mod store;

pub use model::{Post, PostState, Space, Thread, ThreadKind, ThreadPage, ThreadState, User};
pub use path::{Path, PathError, MAX_DEPTH, MAX_ORDINAL, SEGMENT_WIDTH};
pub use store::{Page, Store, StoreError, StoreResult};
