//! Boundary identifier types. The Python engine uses `NewType(str)` for these;
//! they are opaque strings on the wire, so plain aliases keep serde trivial.

pub mod tool_models;
pub mod tool_params;

pub use tool_models::{AgentToolId, ToolEndpoint, ToolId};

pub type FileId = String;
pub type UserId = String;
pub type OwnerId = String;
pub type PrincipalId = String;
