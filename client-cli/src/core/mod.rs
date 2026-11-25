//! Core layer: everything that talks to the controller or to `~/.nordkraft/`
//! and returns typed values. No printing, no prompts, no colour — the
//! terminal (`cli`), `--json` and the MCP server are three presentations
//! over these functions, so they can never disagree about what happened.

pub(crate) mod aliases;
pub(crate) mod api;
pub(crate) mod config;
pub(crate) mod containers;
pub(crate) mod deploy;
pub(crate) mod ingress;
pub(crate) mod registry;
pub(crate) mod spec;
pub(crate) mod usage;

/// Every core call fails with the message the terminal shows the user.
pub(crate) type CoreResult<T> = Result<T, Box<dyn std::error::Error>>;
