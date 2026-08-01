pub mod cli;
pub mod commands;
pub mod deps;
pub mod error;
pub mod exec;
pub mod id;
mod macros;
pub mod model;
pub mod package;
pub mod pipeline;
pub mod register;
pub mod render;
pub mod report;
pub mod spec;
pub mod submissions;

pub use commands::dispatch;
pub use error::{Error, Result};
