//! Build Studio — optional, isolated, downstream of `DecisionRecord`
//! (ARCHITECTURE.md §13). Cannot influence the decision it builds from. Ships in
//! phase 1.5; this crate is scaffolded now so the 1.0 interfaces it depends on are
//! exercised, per §19: "everything in 1.5 sits behind an interface 1.0 defines."
#![forbid(unsafe_code)]
