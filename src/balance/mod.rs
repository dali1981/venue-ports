//! Reading balances (`SPEC.md` §6c, `specs/V6-balances-and-resolution.md`).
//!
//! A consumer that keeps its own books reconciles them against what a venue
//! holds. The `BalanceReader` port is one trait per kind of account, since a
//! chain wallet and an exchange account are asked different questions: an EVM
//! holder's native and token balances at a block the caller names
//! ([`EvmBalanceReader`]), and a spot account's balances with the funds an open
//! order holds ([`SpotBalanceReader`]).
//!
//! Every value is the venue's own number, read at call time and exact: an
//! integer on EVM, a `Decimal` on an exchange. Nothing is cached, summed or
//! valued (§2). Reads carry no `Provenance`: nothing is sent.

use crate::dex::ChainAmount;
use crate::evm::BlockTag;
use alloy_primitives::Address;
use anyhow::Result;
use async_trait::async_trait;
use rust_decimal::Decimal;

mod evm;
mod stub;

pub use evm::EvmBalances;
pub use stub::{EvmBalanceCall, EvmBalanceStub, SpotBalanceStub};

/// What an EVM wallet or contract holds, as the node reports it at a block.
///
/// An amount above `u128` ([`ChainAmount`]) is an error naming what was read,
/// never truncated. A read at a past block needs a node that serves that
/// block's state (an archive node, or a fork that holds the history): one that
/// cannot is an `Err`, never the latest value.
#[async_trait]
pub trait EvmBalanceReader: Send + Sync {
    /// `address`'s native balance in wei at `block`.
    async fn native(&self, address: Address, block: BlockTag) -> Result<ChainAmount>;

    /// `holder`'s `balanceOf` of `token` at `block`.
    async fn token(&self, token: Address, holder: Address, block: BlockTag) -> Result<ChainAmount>;

    /// A short, stable label for logging, e.g. `"evm-balances"`.
    fn label(&self) -> &'static str;
}

/// One asset of a spot account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotBalance {
    pub asset: String,
    /// What the account can spend now.
    pub free: Decimal,
    /// What an open order holds. Still the account's.
    pub locked: Decimal,
}

/// What a spot account holds, as the venue reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotAccountBalances {
    /// Only the assets the account holds some of: an asset that is absent is
    /// held at zero.
    pub balances: Vec<SpotBalance>,
    /// The venue's own time of the account's last change (`updateTime`), in
    /// Unix ms. It is not the time of the read.
    pub update_time_ms: Option<u64>,
}

impl SpotAccountBalances {
    /// `asset`'s balance, or `None` when the account holds none of it.
    pub fn balance(&self, asset: &str) -> Option<&SpotBalance> {
        self.balances.iter().find(|balance| balance.asset == asset)
    }
}

/// What an exchange spot account holds. A failed read is an `Err`; nothing was
/// sent.
#[async_trait]
pub trait SpotBalanceReader: Send + Sync {
    async fn balances(&self) -> Result<SpotAccountBalances>;

    /// A short, stable label for logging, e.g. `"binance-spot-balances"`.
    fn label(&self) -> &'static str;
}
