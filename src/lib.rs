//! Kinetix: a multi-protocol LLM proxy.
//!
//! OpenAI Chat Completions and Anthropic Messages in (streaming first),
//! admin-configured upstreams out (Gemini, OpenAI-compatible, Anthropic), with
//! virtual keys, account pools with automatic fallback Routes, and cost
//! tracking.
//!
//! The crate is exposed as a library so that integration tests under `tests/`
//! can drive the wire encoders/decoders directly (NFR-5.5 golden wire-output
//! fixtures, FR-9.1 fixture suite) in addition to the unit and torture tests
//! embedded in the modules.

pub mod adapters;
pub mod admission;
pub mod admin;
pub mod alerts;
pub mod alloc;
pub mod api;
pub mod app;
pub mod assets;
pub mod auth;
pub mod bootstrap;
pub mod cli;
pub mod config;
pub mod cost;
pub mod credentials;
pub mod crypto;
pub mod db;
pub mod export;
pub mod frontends;
pub mod limits;
pub mod live;
pub mod logqueue;
pub mod net;
pub mod outbound;
pub mod passthrough;
pub mod paths;
pub mod pipeline;
pub mod plugins;
pub mod pool;
pub mod predicate;
pub mod ratelimit;
pub mod registry;
pub mod router;
pub mod server;
pub mod sse;
#[cfg(test)]
mod torture;
pub mod trace;
pub mod types;
pub mod update;
pub mod validate;
