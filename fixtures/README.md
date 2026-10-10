# Recorded bodies

What a venue answered, one file for each answer, at `fixtures/<venue>/<origin>/<yyyy-mm-dd>/` for a
recording and `fixtures/<venue>/documented/` for a body copied from a venue's documentation. The
catalogue in `docs/responses/` has a row for every file, with its origin and the test that parses it
(`catalogue_matches_fixtures`).

A recording is a venue's reply with **one** edit: the account's `uid` is replaced by `REDACTED-UID`.
Every other byte is as the venue sent it. A body copied from documentation keeps the documentation's `//`
annotations (`.jsonc`); the test that reads it cuts them.

A **production** recording does not belong here while the repository is public: it holds the account's balances,
permissions, commission rates, order ids and addresses, and the `uid` is the only thing redacted (`SPEC.md` §7).
It is kept in the consumer's private repository.

`binance-spot/testnet/2026-10-07/` is the one recording so far: the spot testnet's replies to a read-only and a two-order session
(see its `README.md`). No production recording is here.
