# EVM nodes: what each call answers, and where this crate has seen it

The catalogue of `docs/responses/binance-spot.md`, for the JSON-RPC calls `EvmRpc` and `EvmSender` make.
Origins and the rule that keeps the table honest are the same; read them there.

**State of the recordings.** No node's reply is recorded in this repository yet. The production tier
(`production_lv1_base`, `specs/V7-production-validation.md` case C1–C6) records the wording of each provider
it is run against; until it has been run, the bodies below are the ones the tests write inline.

| Call | Variant | Body | Origin | Test | Typed result |
| --- | --- | --- | --- | --- | --- |
| `eth_sendRawTransaction` | `-32000 insufficient funds for gas * price + value` | — | synthetic | `evm::tx::tests::a_broadcast_the_node_refuses_is_an_error_and_leaves_nothing_unresolved` | an `Err` naming the node's wording; nothing left unresolved |
| a mined revert | a transaction that reverted in a block | — | not provokable | — | — |
| a dropped or replaced transaction | the node no longer knows the hash | — | not provokable | — | — |

## The reads of V7 and the cases of the production tier

As in `docs/responses/binance-spot.md`: **no body here is a file**; each is built in the test that names it,
and none was said by a node to this crate. The production tier records each provider's wording when a person
runs it (`VP_RECORD_DIR`); these rows become `production` then.

| Call | Variant | Body | Origin | Test | Typed result |
| --- | --- | --- | --- | --- | --- |
| `eth_getTransactionReceipt` | a receipt with `transactionIndex` | — | synthetic | `evm::rpc::tests::a_receipt_carries_where_its_transaction_sits_in_the_block` | `Receipt.transaction_index` |
| `eth_getTransactionReceipt` | a receipt with no `transactionIndex`, and the first transaction of a block (`0x0`) | — | synthetic | `evm::rpc::tests::a_receipt_without_an_index_has_none_and_the_first_transaction_is_zero` | `transaction_index: None`, and `Some(0)` |
| `eth_getTransactionReceipt` | a `transactionIndex` that is not a hex quantity | — | synthetic | `evm::rpc::tests::an_index_that_is_not_a_hex_quantity_is_an_error` | an `Err`, not a guess |
| `eth_getBlockTransactionCountByNumber` | a block's length, at the block named | — | synthetic | `evm::rpc::tests::a_blocks_transaction_count_is_read_at_the_block_named` | the count |
| `eth_getBlockTransactionCountByNumber` | a block the node does not have (`null`) | — | synthetic | `evm::rpc::tests::a_block_the_node_does_not_have_has_no_count` | an `Err` naming the block, not a count of zero |
| `eth_sendRawTransaction` | an unfunded signer's zero-value transfer (C1) | — | synthetic | `production::lv1_base::tests::c1_an_unfunded_signer_is_refused_and_the_wording_is_recorded` | an `Err` that is not a revert; the wording recorded |
| `eth_estimateGas` | a transfer that reverts at the estimate (C2) | — | synthetic | `production::lv1_base::tests::c2_a_transfer_that_reverts_at_the_estimate_is_not_broadcast` | `RpcError::is_revert()`; nothing broadcast |
| `eth_chainId` | a node on another chain (C3) | — | synthetic | `production::lv1_base::tests::c3_the_wrong_chain_id_is_refused_at_connect_naming_both` | an `Err` from `connect` naming both ids |
| `eth_getBlockByNumber("pending")`, `eth_call` at `pending`, `eth_maxPriorityFeePerGas`, an over-wide `eth_getLogs` (C4) | what a provider serves, one request at a time | — | synthetic | `production::lv1_base::tests::c4_four_requests_one_at_a_time_each_recorded_and_none_judged` | each reply's shape recorded, not judged |
| a dead node | a closed port; a node that sleeps past the timeout (C5) | — | synthetic | `production::lv1_base::tests::c5_a_closed_port_and_a_node_that_sleeps_are_errors_and_not_a_nodes_answer` | an `Err` that is not a revert |
| a pool's views | `stable()`, `getFee(pool,bool)`, `getReserves()`, `code(pool)` (C6; the ABI is from memory, `specs/V7-questions.md` Q3) | — | synthetic | `production::lv1_base::tests::c6_the_pools_views_decode_and_the_code_is_hashed` | the raw replies decode |
| a router swap on a stateful mock chain | a swap pair, with the `Transfer` logs of other tokens and accounts among the swap's (LV2b) | — | synthetic | `production::lv2b_chain::tests::a_swap_pair_lands_and_all_six_checks_pass_with_the_references_of_the_table` | `Realised` equal to the receipt's own record, X1 to X6 |
| a router swap that reverts | `Pool: LOCKED`, and a delivery under `amountOutMin` (LV2b) | — | synthetic | `production::lv2b_chain::tests::a_swap_that_reverts_records_its_reason_and_skips_the_amount_checks` | `Outcome::Reverted { reason }`; X1 and X2 skipped |
| a swap that is accepted and never mined | no receipt before the timeout (LV2b) | — | synthetic | `production::lv2b_chain::tests::a_swap_that_is_never_mined_is_a_timeout_with_null_references` | `Outcome::TimedOut`; the landing's references `null` |
