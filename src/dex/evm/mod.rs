//! EVM-family `DexExecutor` implementations. See `SPEC.md` §5 and
//! `IMPLEMENTATION_PLAN.md` Phases 4, 5 and 10. The plumbing they share —
//! the RPC client, ERC-20 helpers and the one sender per wallet — lives in
//! [`crate::evm`].

mod live;
mod simulated;

pub use live::EvmLive;
pub use simulated::{CodeOverride, EvmSimulated, ReturnRule};
