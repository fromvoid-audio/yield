//! Profile-local durable operation-journal values.
//!
//! This module currently exposes the stable identity, intent, phase, and disposition model. It
//! does not open files, acquire ownership, publish filesystem entries, or interpret journal data
//! as mutation authority.

pub mod model;

pub use model::{
    CURRENT_SCHEMA_VERSION, OperationActor, OperationDisposition, OperationId, OperationIntent,
    OperationKind, OperationPhase, SCHEMA_V1, SCHEMA_V2,
};
