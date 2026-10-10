//! `EvmBalanceStub` and `SpotBalanceStub` — in-process fakes of the balance
//! readers (`SPEC.md` §6c): programmable values and errors, and a record of
//! every call, the same shape as `CexAccountStub`.
//!
//! Neither invents a value. A balance that was never set is an error, not a
//! made-up zero.

use crate::balance::{EvmBalanceReader, SpotAccountBalances, SpotBalanceReader};
use crate::dex::ChainAmount;
use crate::evm::BlockTag;
use alloy_primitives::Address;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

/// One call exactly as the stub received it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvmBalanceCall {
    Native {
        address: Address,
        block: BlockTag,
    },
    Token {
        token: Address,
        holder: Address,
        block: BlockTag,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Account {
    Native(Address),
    Token { token: Address, holder: Address },
}

/// A history of balances by block, as a chain keeps one: a value set at block
/// `b` holds until a later block sets another.
#[derive(Default)]
pub struct EvmBalanceStub {
    history: Mutex<BTreeMap<(Account, u64), ChainAmount>>,
    errors: Mutex<VecDeque<anyhow::Error>>,
    calls: Mutex<Vec<EvmBalanceCall>>,
}

impl EvmBalanceStub {
    pub fn new() -> Self {
        Self::default()
    }

    /// `address` holds `amount` wei from `block` on, until a later block sets
    /// another.
    pub fn set_native(&self, address: Address, block: u64, amount: ChainAmount) {
        self.history
            .lock()
            .unwrap()
            .insert((Account::Native(address), block), amount);
    }

    /// `holder` holds `amount` of `token` from `block` on, until a later block
    /// sets another.
    pub fn set_token(&self, token: Address, holder: Address, block: u64, amount: ChainAmount) {
        self.history
            .lock()
            .unwrap()
            .insert((Account::Token { token, holder }, block), amount);
    }

    /// The next read fails with `error` instead of answering. Errors are
    /// consumed in the order programmed.
    pub fn program_error(&self, error: anyhow::Error) {
        self.errors.lock().unwrap().push_back(error);
    }

    /// Every call the stub has received so far, in the order received.
    pub fn calls(&self) -> Vec<EvmBalanceCall> {
        self.calls.lock().unwrap().clone()
    }

    fn read(&self, call: EvmBalanceCall, account: Account, block: BlockTag) -> Result<ChainAmount> {
        self.calls.lock().unwrap().push(call);
        if let Some(error) = self.errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        let up_to = match block {
            BlockTag::Number(number) => number,
            BlockTag::Latest | BlockTag::Pending => u64::MAX,
        };
        self.history
            .lock()
            .unwrap()
            .range((account, 0)..=(account, up_to))
            .next_back()
            .map(|(_, amount)| *amount)
            .ok_or_else(|| {
                anyhow!("no balance was set on this stub for {account:?} at or before {block:?}")
            })
    }
}

#[async_trait]
impl EvmBalanceReader for EvmBalanceStub {
    async fn native(&self, address: Address, block: BlockTag) -> Result<ChainAmount> {
        self.read(
            EvmBalanceCall::Native { address, block },
            Account::Native(address),
            block,
        )
    }

    async fn token(&self, token: Address, holder: Address, block: BlockTag) -> Result<ChainAmount> {
        self.read(
            EvmBalanceCall::Token {
                token,
                holder,
                block,
            },
            Account::Token { token, holder },
            block,
        )
    }

    fn label(&self) -> &'static str {
        "evm-balance-stub"
    }
}

#[derive(Default)]
pub struct SpotBalanceStub {
    balances: Mutex<Option<SpotAccountBalances>>,
    errors: Mutex<VecDeque<anyhow::Error>>,
    calls: Mutex<usize>,
}

impl SpotBalanceStub {
    pub fn new() -> Self {
        Self::default()
    }

    /// What `balances()` returns, until replaced.
    pub fn set_balances(&self, balances: SpotAccountBalances) {
        *self.balances.lock().unwrap() = Some(balances);
    }

    /// The next read fails with `error` instead of answering. Errors are
    /// consumed in the order programmed.
    pub fn program_error(&self, error: anyhow::Error) {
        self.errors.lock().unwrap().push_back(error);
    }

    /// How many reads the stub has received.
    pub fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

#[async_trait]
impl SpotBalanceReader for SpotBalanceStub {
    async fn balances(&self) -> Result<SpotAccountBalances> {
        *self.calls.lock().unwrap() += 1;
        if let Some(error) = self.errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        self.balances
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("no balances were set on this stub"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::balance::SpotBalance;
    use rust_decimal::Decimal;

    const A: Address = Address::new([0xAA; 20]);
    const TOKEN: Address = Address::new([0x11; 20]);

    #[tokio::test]
    async fn a_value_holds_from_its_block_until_a_later_block_sets_another() -> Result<()> {
        let stub = EvmBalanceStub::new();
        stub.set_native(A, 10, 1_000);
        stub.set_native(A, 20, 400);

        assert!(stub.native(A, BlockTag::Number(9)).await.is_err());
        assert_eq!(stub.native(A, BlockTag::Number(10)).await?, 1_000);
        assert_eq!(stub.native(A, BlockTag::Number(19)).await?, 1_000);
        assert_eq!(stub.native(A, BlockTag::Number(20)).await?, 400);
        assert_eq!(stub.native(A, BlockTag::Latest).await?, 400);
        Ok(())
    }

    #[tokio::test]
    async fn native_and_token_balances_are_kept_apart_per_holder_and_token() -> Result<()> {
        let stub = EvmBalanceStub::new();
        let other = Address::new([0xBB; 20]);
        stub.set_native(A, 1, 7);
        stub.set_token(TOKEN, A, 1, 9);

        assert_eq!(stub.native(A, BlockTag::Latest).await?, 7);
        assert_eq!(stub.token(TOKEN, A, BlockTag::Latest).await?, 9);
        // Never a made-up zero.
        assert!(stub.token(TOKEN, other, BlockTag::Latest).await.is_err());
        assert!(stub.native(other, BlockTag::Latest).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn a_programmed_error_fails_one_read_and_every_call_is_recorded() -> Result<()> {
        let stub = EvmBalanceStub::new();
        stub.set_native(A, 1, 7);
        stub.program_error(anyhow!("the node is down"));

        let err = stub.native(A, BlockTag::Latest).await.unwrap_err();
        assert!(err.to_string().contains("the node is down"));
        assert_eq!(stub.native(A, BlockTag::Number(1)).await?, 7);
        assert_eq!(
            stub.calls(),
            vec![
                EvmBalanceCall::Native {
                    address: A,
                    block: BlockTag::Latest
                },
                EvmBalanceCall::Native {
                    address: A,
                    block: BlockTag::Number(1)
                },
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn the_spot_stub_returns_what_was_set_and_errors_when_nothing_was() -> Result<()> {
        let stub = SpotBalanceStub::new();
        assert!(stub.balances().await.is_err());

        let account = SpotAccountBalances {
            balances: vec![SpotBalance {
                asset: "AERO".to_string(),
                free: Decimal::new(1_050, 2),
                locked: Decimal::ZERO,
            }],
            update_time_ms: Some(5),
        };
        stub.set_balances(account.clone());
        stub.program_error(anyhow!("rate limited"));

        assert!(stub.balances().await.is_err());
        assert_eq!(stub.balances().await?, account);
        assert_eq!(stub.calls(), 3);
        assert_eq!(
            account.balance("AERO").map(|b| b.free),
            Some(Decimal::new(1_050, 2))
        );
        assert_eq!(account.balance("USDT"), None);
        Ok(())
    }
}
