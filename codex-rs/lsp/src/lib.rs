//! A small Language Server Protocol client for the agent: servers over stdio, document sync for files the agent
//! touches, the nine code-navigation operations the model can call, and diagnostics the servers publish.
//!
//! Hosts build an [`LspSettings`] from their configuration, resolve it into a [`ServerTable`], and call the shared
//! [`LspManager`]. Nothing here starts unless a host asks for it.

pub mod client;
pub mod config;
pub mod diagnostics;
pub mod format;
pub mod framing;
pub mod gitignore;
pub mod manager;
pub mod operation;
pub mod uri;

pub use client::LspError;
pub use config::LspLimits;
pub use config::LspSettings;
pub use config::ServerSpec;
pub use config::ServerTable;
pub use diagnostics::DiagnosticDelivery;
pub use diagnostics::DiagnosticLimits;
pub use diagnostics::DiagnosticSink;
pub use manager::FileEvent;
pub use manager::LspContext;
pub use manager::LspManager;
pub use manager::QueryOutcome;
pub use operation::EditorPosition;
pub use operation::Operation;
