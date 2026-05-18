//! Per-terminal "focus this agent's window" adapters.
//!
//! Each [`TerminalAdapter`] matches a parent-application bundle id and knows
//! how to raise the specific tab/session hosting the agent's CLI process.
//! When no adapter claims the bundle id, [`dispatch`] falls back to
//! [`generic::GenericAdapter`], which just brings the hosting app to the
//! front via `NSRunningApplication.activate`.

pub mod adapter;
pub mod generic;
pub mod ghostty;
pub mod iterm2;
pub mod resolver;
pub mod terminal_app;

pub use adapter::{
    AdapterError, FocusContext, ParentApp, TerminalAdapter, default_adapters, dispatch,
};
pub use resolver::resolve_parent_app;
