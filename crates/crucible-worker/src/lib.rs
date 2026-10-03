//! Cloudflare Worker backend of octos-crucible: GitHub login, encrypted
//! uploads to the blob store, eval submission (workflow dispatch), the
//! credential hand-off to GitHub Actions, results and downloads.
//!
//! The Worker never sees plaintext: uploads and model keys arrive sealed to
//! the platform public key, and it holds no private key.
//!
//! Layout: everything except `wasm` is plain Rust exercised by native
//! tests; `wasm` adapts it to the Workers runtime. HTTP contract:
//! `docs/api.md`.

pub mod app;
pub mod authz;
pub mod config;
pub mod github;
pub mod http;
pub mod keys;
pub mod model;
pub mod session;
pub mod shard;
pub mod util;

#[cfg(target_arch = "wasm32")]
mod wasm;
