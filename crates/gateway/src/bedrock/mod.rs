//! Where a Bedrock upstream's signing credentials come from.
//!
//! What Bedrock needs on the wire — SigV4 signing, unframing AWS eventstream
//! into SSE, the runtime and control-plane addresses, the model catalog — is
//! `tw_bedrock`, shared with the desktop gateway. Converting Converse to and
//! from the other formats is `tw_dialect::bedrock`. What is left here is the
//! part only this side has: keys from the provider row, or else the instance
//! role through IMDSv2.

pub mod sigv4;
