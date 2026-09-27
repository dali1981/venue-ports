//! EVM-family `DexExecutor` implementations. See `SPEC.md` §5 and
//! `IMPLEMENTATION_PLAN.md` Phases 3–5 for what lands in each submodule.

mod live;
mod simulated;
mod stub;
mod tx;

pub use stub::{EvmStub, RecordedCall};
