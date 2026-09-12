//! Moved to thinkwatch-core (`tw-crypto`). This file is a re-export so
//! the rest of the enterprise tree keeps compiling unchanged.
//!
//! **One copy, not two.** The envelope layout is security-critical and
//! it already drifted once while both copies existed — `json_secret`
//! had diverged by 61 lines before this was collapsed.

pub use tw_crypto::crypto::*;
