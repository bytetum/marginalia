//! Marginalia MVP: save articles and highlights, discuss and analyse them with an agent,
//! keep the synthesis products, and serve everything over REST and MCP.

pub mod domain;
pub mod repo;

/// The version this build reports (`marginalia --version`). Release builds set `MARGINALIA_VERSION` at compile time
/// (nightlies are `0.1.1-nightly.<UTC timestamp>`); any other build reports the Cargo package version.
pub const VERSION: &str = match option_env!("MARGINALIA_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

// Adapters
pub mod blob;
pub mod mem;
pub mod surreal;

// App
pub mod agent;
pub mod extract;
pub mod render;
pub mod service;

// Access
pub mod api;
pub mod mcp;
