//! Original-owner pre-intent collection. Memory custody never supplies decision authority.
mod credit;
pub(crate) use credit::MAX_BYTES as HANDSHAKE_MAX_BYTES;
pub(crate) use credit::{HandshakeBudget, HandshakeCredit};
mod input;
pub use input::{SourceInput, SourcePreparation};
mod plan;
pub use plan::{WalPreparationError, WalPreparationInput};
mod collector;
mod current;
pub(super) use collector::Collector;
pub use collector::{
    WalPreparationHandle, WalPreparationObservation, WalPreparationStatus, WalPreparedDatabase,
};
pub(crate) use current::{CurrentCustody, CurrentRequest, CurrentRound};
