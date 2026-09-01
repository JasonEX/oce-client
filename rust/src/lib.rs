pub mod cli;
pub mod config;
pub mod context;
pub mod filesystem;
pub mod http;
pub mod identity;
pub mod ignore_rules;
pub mod indexer;
pub mod mcp;
pub mod state;
pub mod watcher;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
