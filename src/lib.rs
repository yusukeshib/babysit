//! babysit as a library.
//!
//! Historically babysit was a binary-only crate; the CLI in `main.rs` dispatched
//! straight into these modules. Exposing them here lets other Rust programs
//! (notably `looop`) drive the worker fleet IN-PROCESS instead of shelling out
//! to the `babysit` binary and re-parsing its JSON. The bin (`main.rs`) now
//! consumes this same library, so there is a single source of truth.
//!
//! Everything is reached through a [`Babysit`] context — an explicit handle to a
//! state root. The library never reads the environment to find its root; the
//! embedder passes it to [`Babysit::new`]. This keeps the library a pure
//! function of its inputs: no `$BABYSIT_DIR` action-at-a-distance.

pub mod attach;
pub mod cli;
pub mod control;
pub mod pane;
pub mod paths;
pub mod render;
pub mod run;
pub mod session;
pub mod sub;
#[cfg(feature = "upgrade")]
pub mod upgrade;

pub use paths::Babysit;
pub use session::SessionInfo;
