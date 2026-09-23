//! diet_soda's reusable runtime. The TUI is an event consumer, not the owner
//! of model calls, approvals, persistence, or workflow execution.
//!
//! Start with [`config::Config`], [`session::Session`], and [`engine::Engine`].
//! Tests exercise these services using loopback fixtures without API credentials.
pub mod config;
pub mod engine;
#[doc(hidden)]
pub mod fsutil;
pub mod hooks;
pub mod init;
pub mod mcp;
pub mod model;
pub mod process;
pub mod provider;
pub mod session;
pub mod skills;
pub mod template;
pub mod text;
pub mod tools;
pub mod tui;
#[cfg(windows)]
pub(crate) mod winjob;
pub mod workflow;
