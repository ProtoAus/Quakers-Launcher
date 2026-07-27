//! quakers launcher core.
//!
//! A small, reusable library that mirrors a content-addressed manifest onto disk:
//! fetch manifest -> diff against local files -> resumable parallel download ->
//! verify by hash -> repair. Kept UI-agnostic enough that a GUI can wrap it later;
//! the terminal front-end lives in `main.rs`.

pub mod config;
pub mod download;
pub mod hashing;
pub mod manifest;
pub mod plan;
pub mod state;
pub mod ui;
