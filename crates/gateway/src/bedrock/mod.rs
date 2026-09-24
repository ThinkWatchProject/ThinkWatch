//! What only Bedrock needs on the wire: SigV4 request signing and
//! unframing AWS eventstream into SSE.
//!
//! Converting Converse to and from the other formats is not here — that is
//! `tw_dialect::bedrock`, shared with the desktop gateway. The desktop
//! gateway does not talk to Bedrock, so these two live on this side only.

pub mod eventstream;
pub mod sigv4;
