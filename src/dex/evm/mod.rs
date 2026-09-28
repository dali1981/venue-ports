//! EVM-family `DexExecutor` implementations. See `SPEC.md` §5 and
//! `IMPLEMENTATION_PLAN.md` Phases 3–5 and 10. The plumbing they share —
//! the RPC client, ERC-20 helpers and the one sender per wallet — lives in
//! [`crate::evm`].

mod live;
mod simulated;
mod stub;

pub use live::EvmLive;
pub use simulated::EvmSimulated;
pub use stub::{EvmStub, RecordedCall};
