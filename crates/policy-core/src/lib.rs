//! Guest-side policy rules: catalog lookup, validation, leases, and the
//! host-testable scheduler model. This crate does not perform I/O.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod canonical;
pub mod catalog;
pub mod engine;
pub mod entries;
pub mod heuristic;
pub mod resources;
pub mod sched;

pub use canonical::{hash_snapshot, write_snapshot_canonical, CanonicalError};
pub use catalog::{catalog_spec, CatalogSpec, ProfileParams};
pub use engine::{
    Ack, AckKind, ActivateOutcome, Capture, EngineConfig, PolicyEngine, PolicyError, PolicyStatus,
    ProposalOutcome,
};
pub use heuristic::{choose_heuristic, HeuristicThresholds, HEURISTIC_V0};
pub use sched::{FairScheduler, SchedError};
