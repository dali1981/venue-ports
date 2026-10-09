# V7: questions

Open questions for whoever owns the spec, found while building it. Each says what was done meanwhile.

## Q1. The testnet recording is not in the repository

Step 2 says to copy "the consumer's recording of Binance's spot testnet (7 October 2026, 34 files,
164 KB, the account's `uid` already replaced by `REDACTED-UID`)" to
`fixtures/binance-spot/testnet/2026-10-07/`, and to replace the six `TODO(R2)` test bodies by it. The
recording is not in this repository, on any branch, or in the build environment, and this crate has no route
to the testnet to record one. It was **not** reconstructed or invented.

Done instead:

- The six `TODO(R2)` bodies are files under `fixtures/binance-spot/documented/`, read with `include_str!`,
  byte for byte what the tests held. The markers are gone from the source because the gap now lives where
  the spec puts it, the catalogue: each is a row of `docs/responses/binance-spot.md` with origin
  `documented`.
- `catalogue_matches_fixtures` is in (`src/catalogue.rs`).

To close it: copy the recording to `fixtures/binance-spot/testnet/2026-10-07/` with its README, add a row
for each file (origin `testnet`, naming a test that reads it), point each of the six tests at the recorded
body where one exists, and fix any parser a recorded body disagrees with.

## Q2. The testnet recording closes more than the six `TODO(R2)` bodies

The same recording would let `docs/responses/binance-spot.md` hold `testnet` rows and let B6, B7, B9 and B10 have
recorded self-test bodies. Until it is supplied, every LV1 and LV2a mock answers with documented or synthetic
bodies, and the catalogue says so.

## Q3. The Aerodrome router's ABI is from memory (LV2b)

`lv2b_chain::swap_calldata` and `quote_calldata` encode `swapExactTokensForTokens(uint256 amountIn, uint256
amountOutMin, (address from, address to, bool stable, address factory)[] routes, address to, uint256 deadline)` and
`getAmountsOut(uint256, Route[])`, with `stable()` and `factory()` read from the pool and `decimals()` from the
tokens. The spec says to check the router's verified ABI before coding and, if it differs, stop. This environment
has no block explorer, so **it was not checked**. Check the verified source of the router at `VP_ROUTER`, and
change the two functions if it differs. The self-tests share these definitions with the mock, so they cannot catch it.

## Q4. How the account's commission rate becomes a trade's commission (LV2a, E4)

`account_commission` gives `standard`, `special` and `tax` blocks, each with `maker`, `taker`, `buyer`, `seller`.
The documentation does not say how they combine. `exchange_checks::effective_rate` adds the taker rate and the
side's own (`buyer` for a buy, `seller` for a sell) of all three blocks. E4 applies the rate to the amount
**received** (a buy's quantity; a sell's quantity times price), to eight decimals, and accepts a difference of one
unit of the last decimal, because the venue's rounding there is not documented (the pass says so). If the first
production run fails E4 with a plausible number, the formula or the rounding is the suspect, not the venue.

## Q5. E5 is exact, and a symbol whose price times quantity has more than the quote asset's decimals could fail it

E5 compares each asset's change to `Σ price × quantity` of the trade lines, exactly. For AEROUSDT (price 4
decimals, step 0.1) the product has 5 decimals and is exact. A symbol whose product exceeds the quote asset's scale
may be rounded by the venue, and E5 would then fail by the rounding. Not seen; flagged.

## Q6. `DRY_RUN` in the spec's "Done when" is `VP_DRY_RUN`

The harness table and every ignored test use `VP_DRY_RUN=1`. Amend the "Done when" line.

## Q7. LV2b's ledger use

A swap is `authorise`d by the same rule as an exchange order: the loss so far plus the swap's whole notional must not
pass `VP_SPEND_CAP_USD`, so a cap below one swap plus the run's losses stops the run early. The loss is the wallet's
value (native at `VP_NATIVE_USD`, the input token at a dollar a unit, the output token at what the router would give
for all of it) read before the run and after each swap, booked with `record_loss`. `VP_TOKEN_IN` must be USD-pegged.
`amount_out_vs_quote_bps` is `(amount_out − quote) / quote` in basis points, not "of the mid".

## Q8. DEFECT, reported and not fixed: `inclusion_block` of a swap that never landed

`chain_checks::swap_references` writes `"inclusion_block": realised.map(|r| r.at)`. For a swap that ended
`TimedOut`, `EvmLive` sets `Realised.at` to `rpc.block_number().unwrap_or(0)`: the latest block when it gave up
waiting, **not** a block the swap was included in, and `0` when that read failed. So a swap that was never mined
carries a block number (or zero) as its inclusion block, where the spec says a reference that does not exist is
`null`, never zero (`specs/V7-production-validation.md`, "What this does not do").

Found while writing the mock-chain test of a swap that is accepted and never mined
(`a_swap_that_is_never_mined_is_a_timeout_with_null_references`), which asserts every other landing reference is
`null` and leaves this one out for this reason. A consumer reading `inclusion_block` to find the landing block
would read the block at which the run stopped waiting.

Seen, not only read: on the mock chain, with the swap accepted and never mined, the line's `references` has
`"inclusion_block": 1001`, which is the block of the swap's approval, the latest when the run gave up.

Proposed fix, **not made**: `inclusion_block` is `Some(at)` only when the outcome is `Success` or `Reverted`;
a test that fails first (a timed-out swap's `inclusion_block` is `null`), in its own commit. Waiting for the
owner, as the working agreement says.

## Q9. A dry run of LV2b prints one call

`VP_DRY_RUN=1` stops LV2b at its first read (`web3_clientVersion`): a dry run makes no read, and a swap is sized
from reads, so none of the run's other calls (the pool's views, the quote, the approval, the swap) is printed.
LV2a prints the three reads that start its run and then skips each order for the same reason. The spec's "Done
when" says a dry run "prints its calls and sends none"; for LV2a and LV2b this holds for the reads that need no
other read, and no more. Nothing was changed to print a swap that was not sized.
