//! Library face of `think-watch-server`. Re-exports the modules
//! needed by integration tests in `crates/test-support` and any other
//! out-of-process consumer that wants to mount the same Axum routers
//! as the production `main.rs`.

pub mod app;
pub mod handlers;
pub mod init;
pub mod mcp_runtime;
pub mod middleware;
pub mod oidc_helpers;
pub mod openapi;
pub mod services;
pub mod tasks;
pub mod tracing_ch;
