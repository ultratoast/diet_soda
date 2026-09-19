//! Diet Harness's reusable runtime. The TUI is an event consumer, not the owner
//! of model calls, approvals, persistence, or workflow execution.
//!
//! Start with [`config::Config`], [`session::Session`], and [`engine::Engine`].
//! Tests exercise these services using loopback fixtures without API credentials.
pub mod config;
pub mod engine;
pub mod hooks;
pub mod mcp;
pub mod model;
pub mod process;
pub mod provider;
pub mod session;
pub mod skills;
pub mod template;
pub mod tools;
pub mod tui;
pub mod workflow;
