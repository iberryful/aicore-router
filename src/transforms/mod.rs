//! Per-family request body transforms.
//!
//! Each submodule shapes outgoing JSON for one upstream provider's request format.
//! The dispatcher sits in `proxy::prepare_body`; see each submodule's doc-comments
//! for the source-of-truth references.

pub mod anthropic;
pub mod gemini;
pub mod openai;
pub mod openai_responses;
pub mod stream_classify;

pub use anthropic::extract_anthropic_beta;
