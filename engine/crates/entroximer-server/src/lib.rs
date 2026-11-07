//! The Entroximer engine: configuration, track sources and the commands of the `entroximer`
//! binary.

// proving axum handler futures `Send` walks deep into songbird's types
#![recursion_limit = "256"]

pub mod api;
pub mod cache;
pub mod config;
mod expiring;
pub mod managed_ytdlp;
pub mod node;
pub mod render;
pub mod serve;
pub mod sources;
pub mod uploads;
pub mod view;
pub mod youtube;
