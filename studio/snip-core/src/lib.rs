//! Shared core for Snippet Studio frontends.
//!
//! - [`yamlfile`] edits one match file by splicing text, so comments and
//!   formatting outside the edited item never change.
//! - [`workspace`] lists, reads, edits and validates files in an espanso
//!   folder. Every write is checked with espanso's own loader first.
//! - [`rpc`] serves the same operations as JSON-RPC for non-Rust frontends.
//! - [`portability`] tells whether a match can be exported to other programs.
//! - [`priority`] finds the file of each match and picks the winner when
//!   matches share a trigger.
//! - [`matchedit`] reads one match into form fields and writes a form
//!   change back as the smallest text change.
//! - [`pieces`] splits a match into paragraphs that the user checks in a
//!   list (`snip: { pick: many }`).

pub mod error;
pub mod matchedit;
pub mod pieces;
pub mod portability;
pub mod priority;
pub mod rpc;
pub mod workspace;
pub mod yamlfile;

pub use error::{Problem, Result, SnipError};
pub use workspace::Workspace;
