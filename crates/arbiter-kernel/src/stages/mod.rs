//! Concrete `Stage` implementations, one module per pipeline stage
//! (ARCHITECTURE §5). `stage.rs` stays pure infrastructure (the `Stage` trait,
//! `StageContext`, the idempotency-key formula); this module tree is where each
//! `G2`–`G9` task lands its own stage as it is implemented.

pub mod claims_extract;
pub mod claims_normalize;
pub mod options_cluster;
pub mod positions_generate;
pub mod relations_analyze;
pub(crate) mod similarity;
