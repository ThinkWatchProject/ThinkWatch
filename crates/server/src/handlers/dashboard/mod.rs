//! Dashboard handlers — stat tiles, live snapshot + WebSocket pusher,
//! top-active-users leaderboard, and per-user layout persistence.
//! Split across files for readability; the public surface stays the
//! same as the old single-file `dashboard.rs`.
//!
//! - [`stats`]       `GET /api/dashboard/stats`
//! - [`live`]        `GET /api/dashboard/live` + shared snapshot builder
//! - [`top_users`]   `GET /api/dashboard/top-users` + cache
//! - [`layout`]      `GET / PUT /api/dashboard/layout`
//! - [`ws`]          WebSocket ticket + push loop + revoke key
//! - [`scope`]       shared RBAC user-filter resolver

// Submodules are `pub` because the `#[utoipa::path]` macro generates
// `__path_<handler>` companion types alongside each handler. The
// OpenAPI derive in `crate::openapi` references them via the leaf
// module path, so the leaves must stay reachable even though the
// re-exports below are the only thing application code uses.
pub mod layout;
pub mod live;
mod scope;
pub mod stats;
pub mod top_users;
pub mod ws;

// Public surface — flat re-exports so callers' import paths
// (`handlers::dashboard::X`) stay the same after the split.
pub use layout::{DashboardLayout, get_dashboard_layout, put_dashboard_layout};
pub use live::{DashboardLive, LiveLogRow, ProviderHealth, RpmBucket, get_dashboard_live};
pub use stats::{DashboardStats, get_dashboard_stats};
pub use top_users::get_top_active_users;
pub use ws::{WsTicketResponse, create_dashboard_ws_ticket, dashboard_ws, user_revoked_key};

// `#[utoipa::path]` generates a `__path_<handler>` companion type
// alongside each handler. The `OpenApi` derive in `crate::openapi`
// references each handler by `crate::handlers::dashboard::<name>` and
// looks for `__path_<name>` in the same module — re-export them here
// so the derive resolves them after the split.
#[allow(non_camel_case_types, unused_imports)]
pub use layout::{__path_get_dashboard_layout, __path_put_dashboard_layout};
#[allow(non_camel_case_types, unused_imports)]
pub use live::__path_get_dashboard_live;
#[allow(non_camel_case_types, unused_imports)]
pub use stats::__path_get_dashboard_stats;
#[allow(non_camel_case_types, unused_imports)]
pub use top_users::__path_get_top_active_users;
#[allow(non_camel_case_types, unused_imports)]
pub use ws::__path_create_dashboard_ws_ticket;
