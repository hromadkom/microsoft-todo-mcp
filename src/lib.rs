//! microsoft-todo-mcp — an MCP server exposing only Microsoft To Do.
//!
//! Library + thin binary split so `tests/` can drive internals directly. Every
//! module is `pub` for that reason; `main.rs` is a wrapper around `cli::run`.
//!
//! Reading order for a fresh contributor: `config` → `auth` → `graph` → `tools`.
//! The two invariants worth knowing before touching anything:
//!
//! 1. **`/users/{id}/…` exists nowhere.** Only `/me`. A grep gate enforces it, so
//!    cross-user access is not a policy but an absence.
//! 2. **The inbound MCP bearer is never forwarded to Graph, and a Graph token is
//!    never accepted as MCP auth.** `TokenProvider` reads the token file and knows
//!    nothing about inbound HTTP; the bearer guard runs before any handler.
//!
//! `clippy::unwrap_used` is warned here *and* in `main.rs` — they are separate
//! crate roots, so the attribute does not carry across.
#![warn(clippy::unwrap_used)]

pub mod auth;
pub mod cache;
pub mod cli;
pub mod clock;
pub mod config;
pub mod domain;
pub mod errors;
pub mod graph;
pub mod http;
pub mod logger;
pub mod mcp;
pub mod sem;
pub mod server;
pub mod tools;
