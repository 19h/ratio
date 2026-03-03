//! Ratio — LLM agent orchestrator with reviewer-driven code generation.
//!
//! Ratio orchestrates two roles over the Agent Client Protocol (ACP):
//!
//! - **Reviewer** (the orchestrator itself): accepts an elaborate goal,
//!   establishes the task with enforced constraints, delegates work, and
//!   validates output through iterative review cycles.
//!
//! - **Worker** (an opencode subprocess): the LLM coding agent that
//!   performs the actual implementation work, communicating over ACP via
//!   stdin/stdout.
//!
//! The user can enforce specific tools, coding approaches, and file-path
//! restrictions that the worker must obey. The reviewer checks compliance
//! after every turn and requests revisions if needed.

pub mod config;
pub mod orchestrator;
pub mod protocol;
pub mod subprocess;
pub mod ui;
