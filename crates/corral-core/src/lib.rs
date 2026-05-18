pub mod agent;
pub mod fsevents;
pub mod kqueue;
pub mod proc;
pub mod registry;
pub mod status;
pub mod text;
pub mod trace;
pub mod transcript;

/// Re-exports of the most common types and helpers used across this
/// crate and its consumers. Import as `use corral_core::prelude::*;`.
pub mod prelude {
    pub use crate::agent::{Agent, Tool, discover};
    pub use crate::proc::{
        ProcessId, ProcessIdentity, ProcessKey, ProcessStartTime, claude_transcript_for,
        list_processes_matching, process_args_env, process_cwd, process_open_session_transcript,
        process_start_time, process_tty,
    };
    pub use crate::registry::{Registry, RegistryEvent, SystemEvent};
    pub use crate::status::AgentState;
    pub use crate::transcript::LifecycleEvent;
}
