# liq-sim

Offline core of an **Aave V3 liquidation simulator / plan builder**, written in Rust.

A faithful port of a Python reference implementation. The math is ported
*exactly* — the same order of `f64` operations, the same integer truncations
(Python `int()` == truncation toward zero), the same fraction ladder and
collateral-balance clamping. Correctly-rounded (round-half-even) U256→f64
conversion and exact integer division are implemented explicitly, because
naive float conversion diverges by 1 ulp on values above 2^53 — this port
was validated against the reference on parity fixtures.

**No networking.** Oracle prices, unwind quotes, gas and config words are
inputs; quotes are supplied as recorded values or a deterministic formula
(`Quote::Fixed` / `Quote::Ratio` / `Quote::Fail`).

## Layout

- `src/lib.rs` — the simulation core: account-data and reserve-config
  decoding, liquidation bonus / protocol fee / eMode bonus math, the
  fraction ladder search (`ladder`), pair and full-position simulation.
- `src/main.rs` — a small CLI: reads a JSON request (file argument or
  stdin), writes JSON to stdout.

## Usage

```bash
cargo build --release

echo '{"op":"ladder", ...}' | ./target/release/liq-sim
# or
./target/release/liq-sim request.json
```

Operations: `ladder`, `simulate_pair`, `simulate`, `decode_config`,
`decode_emode`, `decode_account`.

## Dependencies

`alloy-primitives`, `serde`, `serde_json` — nothing else.

## Provenance

Production-line code from a live DeFi liquidation research system,
published as-is. Internal references in comments were generalised for
publication; the logic is unchanged. Comments in the source are in
Ukrainian, the author's working language.
