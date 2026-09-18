//! Pure domain types shared by ARSY components.

pub mod artifact;
pub mod capability;
pub mod config;
pub mod context;
pub mod domain;
pub mod event;
pub mod memory;
pub mod migrate;
pub mod model_profile;
pub mod oauth;
pub mod observer;
pub mod operation;
pub mod orchestration;
pub mod policy;
pub mod projection;
pub mod prompt;
pub mod protocol;
pub mod provider;
pub mod routing;
pub mod secret;
pub mod service;
pub mod sqlite;
pub mod telemetry;
pub mod todo;
pub mod transport;
pub mod validation;

/// Workspace package version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
