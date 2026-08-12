# Known limitations

What a green test suite here does **not** prove. Read this before treating any
part of Fluxora as production-ready.

---

## 1. The TTL suite does not prove the archival recovery flow

**Status: open. Closing it is the acceptance criterion for stage 4.**

### The claim you might read off a green suite

`test::ttl` passes. It contains
`a_year_long_stream_survives_on_keeper_sweeps_alone` and
`an_archived_stream_restores_with_its_accounting_intact`. It is tempting to
conclude "TTL is solved". **It is not.** Roughly half the problem is untested.

### Why

The Soroban SDK's test host runs storage in *recording* mode. In that mode,
reading an expired persistent entry does not fail. The host calls
`handle_maybe_expired_entry`, which silently restores the entry in place with
its data intact and its TTL reset to `min_persistent_entry_ttl`:

```rust
// soroban-env-host-27.0.1/src/host/storage.rs
if live_until < li.sequence_number {
    match durability {
        ContractDataDurability::Temporary  => { /* entry dropped */ }
        ContractDataDurability::Persistent => {
            // recorded as a ReadWrite access, live_until reset to the minimum
        }
    }
}
```

On a real network the sequence is different, and there is a failure in the
middle of it:

| | test host | live network |
|---|---|---|
| read an archived entry | silently restored, invocation proceeds | **transaction fails** |
| recovery | n/a — never failed | caller must resubmit with a `RestoreFootprint` operation |
| after recovery | entry live at minimum TTL | entry live at minimum TTL |

So the tests exercise the *endpoints* of the journey — a live entry before, a
live entry with intact accounting after — and skip the failure in between.

### What the tests therefore do and do not establish

**Do establish:**

- Rent arithmetic is correct: creation funds a stream for its full remaining
  life plus a 30-day buffer, clamped to `max_entry_ttl`.
- Every mutating call re-extends the entry, so an active stream never decays.
- A year-long stream whose rent cannot be bought in one go survives on
  permissionless keeper sweeps, and pays out in full afterwards.
- Crossing the archive/restore boundary preserves every field of the accounting
  — deposit, withdrawals, schedule, status — with the pool still fully backing
  it, and the pooled tokens are never affected by TTL at all.

**Do not establish:**

- That a client hitting an archived stream gets a recoverable, diagnosable
  failure rather than an opaque one.
- That the `RestoreFootprint` footprint we would build is correct and
  sufficient.
- What the restore actually costs.
- That `stream_exists() == false` while `stream_id < stream_count()` is a
  reliable "needs restoring" signal against a real RPC, as the SDK is intended
  to use it.

### Closing it

Stage 4 must, against live testnet: create a stream, let its entry genuinely
archive, observe the real failure, restore it via `RestoreFootprint`, and show
the stream still pays out correctly. Anything less leaves this section open.

### If you are integrating before then

Assume archived streams are reachable and that your first call against one will
fail. Detect it (`stream_exists() == false` with `stream_id < stream_count()`)
and surface a restore action rather than an error toast. Run a keeper against
`batch_extend_ttl` so it rarely comes up.

---

## 2. Resource measurements understate a real deployment

`test::resource_limits` registers contracts **natively**, not as WASM. Wasm
instantiation and execution costs are therefore skipped, so reported
`instructions` are lower than production. Ledger entry counts and event bytes —
the figures `MAX_BATCH_SIZE` is actually derived from — are accurate.

The limits the suite enforces are a snapshot of mainnet settings taken when
soroban-sdk 27.0.5 was published (2026-07-10), not a live query. They can move
under the contract without the tests noticing. Stage 4 should re-measure against
testnet simulation and reconcile.

---

## 3. `MAX_BATCH_SIZE` is calibrated against one token

The cap is bounded by the **contract event budget**, and roughly half of the
per-stream event cost is the *token's* `transfer` event, not Fluxora's
`withdrawn` event. Measured against the Stellar Asset Contract. A SEP-41 token
with a heavier event payload shifts the ceiling down.

The 2x safety factor exists for this reason, but it is a margin, not a proof. An
integrator standardising on an unusual token should re-run
`cargo test resource_limits -- --nocapture` against it.

---

## 4. Not audited

No third-party security audit has been performed. The property tests, the pool
invariant and the randomized sequence suite are evidence of care, not a
substitute for review.

---

## 5. Ledger close time is assumed, not measured

TTL targets convert seconds to ledgers at a nominal 5s close time
(`storage::SECONDS_PER_LEDGER`). Close time is a network property that drifts.
The constant is deliberately conservative — it over-estimates ledgers per unit
time, so entries are funded for longer than strictly needed — but a sustained
slowdown well beyond 5s/ledger would erode the margin. The 30-day buffer and the
keeper path both exist to absorb that.
