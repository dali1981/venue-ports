//! `EvmBalances` — `EvmBalanceReader` over a node (`SPEC.md` §6c).

use crate::balance::EvmBalanceReader;
use crate::dex::ChainAmount;
use crate::evm::{erc20, BlockTag, EvmRpc};
use alloy_primitives::{Address, U256};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;

/// Reads native balances with `eth_getBalance` and token balances with
/// `balanceOf`, each at the block the caller names. A past block needs a node
/// that serves its state.
#[derive(Debug, Clone)]
pub struct EvmBalances {
    rpc: EvmRpc,
}

impl EvmBalances {
    pub fn new(rpc: EvmRpc) -> Self {
        Self { rpc }
    }
}

/// `amount` as a [`ChainAmount`], or an error saying what was read: a balance
/// above `u128` is never truncated.
fn chain_amount(amount: U256, what: impl FnOnce() -> String) -> Result<ChainAmount> {
    ChainAmount::try_from(amount)
        .map_err(|_| anyhow!("{} is {amount}, which does not fit a u128", what()))
}

#[async_trait]
impl EvmBalanceReader for EvmBalances {
    async fn native(&self, address: Address, block: BlockTag) -> Result<ChainAmount> {
        let what = || format!("{address}'s native balance at {block:?}");
        let wei = self
            .rpc
            .balance_at(address, block)
            .await
            .with_context(|| format!("reading {}", what()))?;
        chain_amount(wei, what)
    }

    async fn token(&self, token: Address, holder: Address, block: BlockTag) -> Result<ChainAmount> {
        let what = || format!("{holder}'s balance of {token} at {block:?}");
        let units = erc20::balance_of(&self.rpc, token, holder, block)
            .await
            .with_context(|| format!("reading {}", what()))?;
        chain_amount(units, what)
    }

    fn label(&self) -> &'static str {
        "evm-balances"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evm::erc20::BALANCE_OF_SELECTOR;
    use crate::evm::rpc::{format_u256, hex_data};
    use anyhow::bail;
    use serde_json::{json, Value};
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const HOLDER: Address = Address::new([0xAA; 20]);
    const TOKEN: Address = Address::new([0x11; 20]);

    fn ok(result: Value) -> ResponseTemplate {
        ResponseTemplate::new(200)
            .set_body_json(json!({ "jsonrpc": "2.0", "id": 1, "result": result }))
    }

    async fn requests_for(server: &MockServer, rpc_method: &str) -> Vec<Value> {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|request| request.body_json::<Value>().unwrap())
            .filter(|body| body["method"] == rpc_method)
            .collect()
    }

    /// The balance is read at the block the caller names, as a hex quantity,
    /// and `latest` stays `latest`.
    #[tokio::test]
    async fn a_native_balance_is_read_at_the_block_named() -> Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_getBalance"})))
            .respond_with(ok(json!("0xde0b6b3a7640000")))
            .mount(&server)
            .await;
        let balances = EvmBalances::new(EvmRpc::new(server.uri()));

        assert_eq!(
            balances.native(HOLDER, BlockTag::Number(0x2a)).await?,
            1_000_000_000_000_000_000
        );
        balances.native(HOLDER, BlockTag::Latest).await?;

        let sent = requests_for(&server, "eth_getBalance").await;
        assert_eq!(sent[0]["params"], json!([HOLDER.to_string(), "0x2a"]));
        assert_eq!(sent[1]["params"], json!([HOLDER.to_string(), "latest"]));
        Ok(())
    }

    #[tokio::test]
    async fn a_token_balance_is_balance_of_at_the_block_named() -> Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_call"})))
            .respond_with(ok(json!(format_u256(U256::from(123_456u64)))))
            .mount(&server)
            .await;
        let balances = EvmBalances::new(EvmRpc::new(server.uri()));

        assert_eq!(
            balances.token(TOKEN, HOLDER, BlockTag::Number(100)).await?,
            123_456
        );

        let sent = requests_for(&server, "eth_call").await;
        let call = &sent[0]["params"][0];
        assert_eq!(call["to"], json!(TOKEN.to_string()));
        assert!(call["data"]
            .as_str()
            .is_some_and(|data| data.starts_with(&hex_data(&BALANCE_OF_SELECTOR))));
        assert_eq!(sent[0]["params"][1], json!("0x64"));
        Ok(())
    }

    /// A balance that does not fit a `u128` is an error naming it, not a
    /// truncated figure.
    #[tokio::test]
    async fn a_balance_above_u128_is_an_error_not_a_truncation() -> Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_getBalance"})))
            .respond_with(ok(json!(format_u256(
                U256::from(u128::MAX) + U256::from(1u8)
            ))))
            .mount(&server)
            .await;
        let balances = EvmBalances::new(EvmRpc::new(server.uri()));

        let err = balances.native(HOLDER, BlockTag::Latest).await.unwrap_err();
        assert!(err.to_string().contains("does not fit a u128"), "{err}");
        assert!(err.to_string().contains(&HOLDER.to_string()), "{err}");
        Ok(())
    }

    /// A node that no longer serves the block's state says so, and the reader
    /// does not answer with the latest value instead.
    #[tokio::test]
    async fn a_block_the_node_cannot_serve_is_an_error() -> Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_getBalance"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1,
                "error": { "code": -32000, "message": "missing trie node (path state) is not available" },
            })))
            .mount(&server)
            .await;
        let balances = EvmBalances::new(EvmRpc::new(server.uri()));

        let err = balances
            .native(HOLDER, BlockTag::Number(1))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("missing trie node"), "{err:#}");
        Ok(())
    }

    /// Against a real node: a block is history. The balance at a block read
    /// before a transaction moved it is still what it was after, and `latest`
    /// is what it became. A token is read by `balanceOf` at a block too: here a
    /// contract whose every call returns 42 (`PUSH1 42, PUSH1 0, MSTORE, PUSH1
    /// 32, PUSH1 0, RETURN`).
    ///
    /// Gated on `EVM_ANVIL_RPC_URL` (plain anvil or a fork); a no-op when unset.
    #[tokio::test]
    async fn against_anvil_a_past_blocks_balance_is_read_after_it_moved() -> Result<()> {
        use crate::evm::tx::tests::{anvil, ANVIL_DEV_KEY_0};
        use crate::evm::{EvmSender, FeePolicy, Signer, TxOutcome};

        let Some((rpc, _anvil)) = anvil().await else {
            return Ok(());
        };
        let balances = EvmBalances::new(rpc.clone());
        let sender = EvmSender::connect(
            rpc.clone(),
            Signer::from_private_key_hex(ANVIL_DEV_KEY_0)?,
            rpc.chain_id().await?,
            FeePolicy::default(),
        )
        .await?;
        let account = sender.address();

        let block = rpc.block_number().await?;
        let before = balances.native(account, BlockTag::Number(block)).await?;
        let TxOutcome::Success { .. } = sender
            .send_and_confirm(Address::new([0x77; 20]), Vec::new(), U256::from(1_000u64))
            .await?
        else {
            bail!("a plain transfer on anvil should succeed");
        };

        assert_eq!(
            balances.native(account, BlockTag::Number(block)).await?,
            before,
            "the block read earlier is history"
        );
        let after = balances.native(account, BlockTag::Latest).await?;
        assert!(
            after < before,
            "the transfer and its gas moved {before} to {after}"
        );

        // A token: code the node holds, read through `balanceOf`.
        let token = Address::new([0xC0; 20]);
        rpc.call(
            "anvil_setCode",
            json!([token.to_string(), "0x602a60005260206000f3"]),
        )
        .await?;
        rpc.call("evm_mine", json!([])).await?;
        assert_eq!(balances.token(token, account, BlockTag::Latest).await?, 42);
        let mined = rpc.block_number().await?;
        assert_eq!(
            balances
                .token(token, account, BlockTag::Number(mined))
                .await?,
            42
        );
        Ok(())
    }
}
