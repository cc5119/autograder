//! Single integration test binary -- `init`/`publish`/`cli` are modules
//! here, not separate `tests/*.rs` targets (see `autotests = false` and the
//! `[[test]]` entry in `Cargo.toml`), so `cargo test` compiles and links
//! this suite once instead of once per file.

mod cli;
mod common;
mod fetch;
mod init;
mod pipeline;
mod publish;
