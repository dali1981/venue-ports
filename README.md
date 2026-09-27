# venue-ports

A specification for a Rust crate that provides a **tested, reusable connection layer** between an
automated trading system and the venues it trades on — decentralized exchange (DEX) routers on EVM
and Solana-style chains, and centralized exchange (CEX) trading APIs.

**This repository is a specification only. There is no implementation here yet.** It exists so the
crate can be built once, correctly, and reused by more than one trading project, instead of every
project re-deriving its own signing, order-rounding, and fill-handling code.

Read [`SPEC.md`](SPEC.md) for the full technical specification — architecture, the exact Rust trait
and type signatures to implement, module layout, the three-mode execution model (live / simulated /
stub), the test strategy that keeps the stub honest, and acceptance criteria.

## Scope, in one paragraph

Given a route or an order that something else has already decided to send, this crate is responsible
for turning it into a real (or simulated, or faked) outcome and returning that outcome once it is
known — nothing more. It does not fetch price quotes, decide whether a trade is worth taking, track
positions or capital, or record anything to a database. Those are a trading system's own concerns and
stay out of this crate by design (see "Non-goals" in `SPEC.md`).

## License

MIT — see [`LICENSE`](LICENSE).
