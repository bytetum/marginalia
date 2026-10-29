//! Marginalia MVP: save articles and highlights, discuss and analyse them with an agent,
//! keep the synthesis products, and serve everything over REST and MCP.

pub mod domain;
pub mod repo;

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
