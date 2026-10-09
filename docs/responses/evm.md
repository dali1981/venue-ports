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
