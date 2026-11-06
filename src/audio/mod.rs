//! Sound. For now the 3D world's ambient beds: what the land sounds like as
//! a ride passes through it.
//!
//! No GTK here (CLAUDE.md §2.6): the 3D view says what it shows, and this
//! decides nothing but how to play it.

pub mod beds;
pub mod mix;
mod player;

pub use player::{get, init, next_owner, Owner};
