#![forbid(unsafe_code)]
//! `dedupe` core: all logic, no `cfg`, no `unsafe`.
//!
//! Everything OS-specific lives in a `backend-*` crate behind the safe traits in
//! [`traits`]. This crate must compile identically on every target.

pub mod cancel;
pub mod config;
pub mod db;
pub mod error;
pub mod hash;
pub mod index;
pub mod lookup;
pub mod model;
pub mod path;
pub mod pipeline;
pub mod progress;
pub mod report;
pub mod scheduler;
pub mod storage_profile;
pub mod traits;

pub use error::{CoreError, Result};
