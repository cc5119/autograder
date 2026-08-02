use std::fmt;

use dialoguer::Select;

pub mod init;
pub mod publish;
pub mod stub;
pub mod template;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishMode {
    /// The starter/template repo: `todo!()`-style stubs, reference-only
    /// helpers and adversarial tests stripped out.
    Starter,
    /// The reference solution: real implementations, reference-only
    /// helpers kept; harness's private tests are stripped.
    Solution,
}

impl PublishMode {
    /// The repo-name suffix: `2026-01-hw3-starter` / `2026-01-hw3-sol`.
    pub(crate) fn repo_suffix(&self) -> &'static str {
        match self {
            Self::Starter => "starter",
            Self::Solution => "sol",
        }
    }

    /// Defaults to `Starter`, the one pushed every term; a solution push is
    /// the deliberate exception.
    pub(crate) fn prompt(prompt: &str) -> Result<Self> {
        let options = [Self::Starter, Self::Solution];
        let selected = Select::new()
            .with_prompt(prompt)
            .items(options)
            .default(0)
            .interact()
            .map_err(|source| Error::Other(format!("failed to read push mode: {source}")))?;
        Ok(options[selected])
    }
}

impl fmt::Display for PublishMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PublishMode::Starter => write!(f, "starter"),
            PublishMode::Solution => write!(f, "solution"),
        }
    }
}
