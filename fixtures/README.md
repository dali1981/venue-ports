# Recorded bodies

What a venue answered, one file for each answer, at `fixtures/<venue>/<origin>/<yyyy-mm-dd>/` for a
recording and `fixtures/<venue>/documented/` for a body copied from a venue's documentation. The
catalogue in `docs/responses/` has a row for every file, with its origin and the test that parses it
(`catalogue_matches_fixtures`).

A recording is a venue's reply with **one** edit: the account's `uid` is replaced by `REDACTED-UID`.
Every other byte is as the venue sent it. A body copied from documentation keeps the documentation's `//`
annotations (`.jsonc`); the test that reads it cuts them.

Nothing here is recorded yet: see `specs/V7-questions.md`, Q1.
