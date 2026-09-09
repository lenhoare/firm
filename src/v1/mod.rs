//! Firm v1 — parallel, board-driven delegation.
//!
//! v0 (`app.rs`) runs a strictly sequential cycle in which a manager turn precedes every
//! single unit of work. v1 replaces that with a task board, a pull-based dispatcher, and
//! isolated git worktrees, so several cheap agents work at once and the expensive manager
//! is not in the loop at all. See `project_spec.md`.
//!
//! Milestone 1 covers the board, the dispatcher, worktree isolation and the command
//! scorer, in partition mode. The forum, tiered routing, the arbiter manager, pluggable
//! scorers and compete mode follow.

pub mod board;
pub mod dispatch;
pub mod forum;
pub mod plan;
pub mod scorer;
pub mod worktree;

#[cfg(test)]
mod tests;
