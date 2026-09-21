//! Experimental, inactive audit-journal delivery metadata foundation.
//!
//! This module owns bounded manifest/cursor operations over an already-created
//! private JSONL journal. It neither creates audit bodies nor sends them. The
//! existing recorder and runtime do not call this module; enabling the Cargo
//! feature only makes the Rust API available for integration work in a later
//! change.
//!
//! A store has one process-local owner. It deliberately adds no lifetime file
//! lock: app ownership must eventually come from the existing root runtime
//! lease. Mutations use private staging files, file sync, atomic replacement,
//! and directory sync. A directory-sync failure fences that owner until the
//! store is reopened because publication may already have committed.

mod recovery;
mod state;

pub use state::{
    AcknowledgeResult, AttemptToken, AuditDeliveryError, AuditDeliveryStore, DestinationProfile,
    JournalId, PreparedRange, SegmentId, SegmentStatus,
};

#[cfg(test)]
mod tests;
