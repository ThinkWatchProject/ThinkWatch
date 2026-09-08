//! 已搬到 thinkwatch-core。这里只留再导出，让企业版其余代码不必改动。
//!
//! DTO 与 `CallCtx` 在 `tw-types`；provider 抽象在 `tw-provider`。

pub use tw_provider::{AiProvider, ProviderBase};
pub use tw_types::*;
