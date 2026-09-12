//! notespace core domain model.
//!
//! This crate is deliberately free of I/O, of `wasm`/native awareness, and of any
//! dependency that cannot compile to `wasm32-unknown-unknown`. Everything
//! here is unit-testable without a database or a network.

pub mod cache_key;
pub mod conformance;
pub mod cookie;
pub mod csrf;
pub mod id;
#[cfg(feature = "password")]
pub mod login;
pub mod model;
pub mod moderation;
pub(crate) mod naming;
#[cfg(feature = "password")]
pub mod password;
pub mod path;
pub mod ratelimit;
#[cfg(feature = "password")]
pub mod register;
pub mod reply;
pub mod session;
pub mod space_key;
pub mod sql;
pub mod store;
pub mod username;

pub use id::{IdError, PublicId, ID_CHARS, MAX_TIMESTAMP_MS};
pub use model::{Post, PostState, Role, Space, Thread, ThreadKind, ThreadPage, ThreadState, User};
pub use naming::NameError;
pub use path::{Path, PathError, MAX_DEPTH, MAX_ORDINAL, SEGMENT_WIDTH};
pub use session::{Session, SessionPolicy, SessionToken, TokenHash};
pub use space_key::{SpaceKey, SpacePath, SpacePathError};
pub use store::{Authenticated, Page, PostLocation, Store, StoreError, StoreResult};
pub use username::Username;
