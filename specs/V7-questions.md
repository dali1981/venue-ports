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
