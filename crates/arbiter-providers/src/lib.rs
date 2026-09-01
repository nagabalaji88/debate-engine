//! Speaks to models. Declares capabilities rather than assuming them
//! (ARCHITECTURE.md §7 idempotency, §11.1 credentials). `mock` scripts the whole
//! CI fixture suite and opens no socket, which is what makes CI free.
#![forbid(unsafe_code)]
