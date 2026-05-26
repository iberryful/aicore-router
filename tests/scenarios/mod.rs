//! E2E scenario modules.
//!
//! Each module groups tests by concern, not by family. This keeps related
//! assertions adjacent (e.g. all auth header variants in one file) and
//! makes targeted debugging easier (`cargo test ... auth::`).

#![cfg(feature = "e2e")]

pub mod auth;
pub mod claude;
pub mod db_logging;
pub mod gemini;
pub mod health;
pub mod limits;
pub mod model_resolution;
pub mod openai_chat;
pub mod openai_responses;
pub mod routing;
pub mod streaming;
