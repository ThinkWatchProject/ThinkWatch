//! AI surface route handlers. Each file owns one endpoint plus its
//! format converter (if applicable). Shared pipeline pieces live in
//! the parent `proxy` module.

mod anthropic;
mod chat;
mod models;
mod responses;

pub use anthropic::proxy_anthropic_messages;
pub use chat::proxy_chat_completion;
pub use models::list_models_handler;
pub use responses::proxy_responses;
