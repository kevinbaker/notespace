//! notespace core domain model.
//!
//! This crate is deliberately free of I/O, of `wasm`/native awareness, and of any
//! dependency that cannot compile to `wasm32-unknown-unknown` (DESIGN.md §9). Everything
//! here is unit-testable without a database or a network.

pub mod id;
pub mod model;
pub(crate) mod naming;
pub mod path;
pub mod space_key;
pub mod store;
pub mod username;

pub use id::{IdError, PublicId, ID_CHARS, MAX_TIMESTAMP_MS};
pub use model::{Post, PostState, Space, Thread, ThreadKind, ThreadPage, ThreadState, User};
pub use naming::NameError;
pub use path::{Path, PathError, MAX_DEPTH, MAX_ORDINAL, SEGMENT_WIDTH};
pub use space_key::{SpaceKey, SpacePath, SpacePathError};
pub use store::{Page, Store, StoreError, StoreResult};
pub use username::Username;
