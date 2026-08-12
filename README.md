# Fluxora

**A continuous payment streaming primitive for Soroban.**

Lock tokens once; have them accrue continuously to a recipient over time. The
recipient pulls their accrued balance whenever they like.

Fluxora is the layer other things build on — payroll tools, grant programs,
subscription billing, vesting schedules. The contract is the product.

| | |
|---|---|
| Protocol | 27 (live on testnet and mainnet) |
| SDK | `soroban-sdk` 27.0.5 |
| Rust | 1.97.1, target `wasm32v1-none` |
| Token interface | SEP-41 (USDC on Stellar has **7 decimals**) |
| Contract size | ~47 KB |
| Tests | 140, including property tests and a pool invariant checked after every operation |

---

## Quick start

```bash
cargo test                                    # full suite
cargo test resource_limits -- --nocapture     # print measured resource costs
cargo build --target wasm32v1-none --release  # build the contract
```

---

## Design

### Pull-based, because Stellar has no scheduler

There is no cron, no keeper network, no way for a contract to wake itself up.
Every state change must be triggered by an external transaction. Nothing here
runs in the background: the recipient calls `withdraw` and the contract computes
what they have earned at that instant.

### No on-chain stream discovery

There is deliberately **no per-user index** in storage. A `Vec<u64>` of a
treasury's streams grows without bound, costs rent forever, and blows Soroban's
transaction footprint limit once that treasury has a few hundred recipients. On
chain, a stream is only ever addressed by its `u64` id.

Discovery is an off-chain concern. `create_stream` returns the new id and emits
an event carrying sender, recipient and every schedule field, so an indexer can
answer "show me my streams" without the contract paying rent to remember.

`test::resource_limits::cost_is_independent_of_how_many_streams_exist` states
this as a test: the 153rd stream costs exactly what the 2nd did.

### Immutable guarantees

`cancellable`, `pausable` and `transferable` are fixed at creation and can never
change. Before accepting a stream a recipient can verify that the sender cannot
claw it back, freeze it, or reassign it. A stream that could *become* cancellable
later would be worthless as a guarantee.

For the same reason there is no admin key, no upgrade path, no fee switch and no
global pause. Immutability is what lets another protocol depend on this one.

---

## The accrual model

Everything is expressed against a **stream clock** that stops while the stream is
paused:

```
stream_time(now) = paused_at.unwrap_or(now) - paused_total

elapsed  = clamp(stream_time, start_time, end_time) - start_time
vested   = 0                                    if stream_time < cliff_time
         = deposited * elapsed / duration       otherwise   (rounds down)
```

All arithmetic is checked and every failure is a typed error. Nothing panics on
a numeric edge case.

**Rounding is always down.** Truncating in the recipient's disfavour is correct:
the residue stays in the pool and returns to the sender at settlement, so the
contract can never owe more than it holds.

**The cliff gates, it does not delay.** At `cliff_time` the recipient becomes
entitled to everything accrued *since `start_time`* — not merely what accrues
after the cliff. This is standard vesting semantics and it surprises people.

**Conservation is exact.** For all `t`:

```
vested(t) + refundable(t) == deposited
```

with no dust term. That falls out of computing `vested` from the cumulative
formula rather than by summing per-interval deltas — truncation error is
re-derived from scratch on every call instead of accumulating. The obvious
per-interval implementation, which the existing MVPs use, loses a stroop per
withdrawal and strands it in the pool forever. Verified by property test over
random schedules and withdrawal patterns.

### Pause

Pausing freezes the clock and pushes the effective end forward by the paused
duration. Total value delivered stays constant; the schedule stretches. The
recipient can still withdraw while paused — pausing stops *accrual*, not access.
Freezing earned funds would make pausable streams unacceptable to any serious
recipient.

A stream paused across its cliff does not silently pass the cliff while frozen.

### Cancel

Rather than a second state machine, cancellation rewrites the schedule so the
stream looks like one that has fully matured: `deposited` drops to the amount
vested right now, and `end_time` is pulled back to the current point on the
stream clock. Every later `vested` call clamps to the reduced deposit, so
`withdraw` needs no special-casing at all.

Cancelling before the cliff refunds everything — pre-cliff the recipient's
entitlement is zero by definition.

---

## Decisions

The spec left four questions open. All four are settled, and the reasoning is in
the code where the behaviour lives.

### 1. `top_up` extends the duration; it never raises the rate

```
before:  10_000 over 100 days  ->  100/day, ends day 100
top_up(1_000)
after:   11_000 over 110 days  ->  100/day, ends day 110
```

The per-second rate the recipient agreed to never changes. The alternative —
hold `end_time` and raise the rate — retroactively re-vests elapsed time: a
top-up at the halfway point would instantly increase what is already
withdrawable. Keeping the rate fixed means a top-up can never accelerate or
dilute an existing schedule, which is what makes it safe to accept a stream from
an untrusted sender.

The extension rounds **up**, so the effective rate after a top-up is never
faster than the original.

Topping up a *matured* stream is rejected (`StreamMatured`) rather than silently
making the new funds instantly withdrawable.

### 2. `MAX_BATCH_SIZE = 16`, derived from measurement

The spec's "roughly 200 ledger entry reads" predates protocol 23. Since then
live Soroban state is held in memory, and `disk_read_entries` is usually zero for
a contract touching live state. Measured against protocol 27's real limits:

| limit | 16-stream batch | ceiling |
|---|---|---|
| total footprint (entries) | 43 | 400 |
| write entries | 20 | 200 |
| instructions | ~4.6M | 400M |
| **contract event bytes** | **8,192** | **16,384** |

Entry counts would allow well over a hundred streams per call. The **event
budget** is what binds: each stream emits a `withdrawn` event plus the token
contract's own `transfer` event, roughly 512 bytes between them, so the hard
ceiling is about 32.

Sixteen is that ceiling with a 2x safety factor. The margin matters because the
per-stream event cost depends on the *token's* event payload — a token heavier
than the Stellar Asset Contract used in tests would inflate it, and a cap that
merely fits today would fail on somebody else's token.

Oversized batches are rejected with `BatchTooLarge` rather than failing opaquely
at the network level. The SDK chunks client-side.

### 3. Minimum deposit: `deposited >= duration`

At least one stroop per second. Below that the rate truncates to zero and the
recipient accrues nothing until very late — a real footgun for a treasury
streaming a small grant over a year. The floor excludes nothing realistic: a
year-long USDC stream needs only ~3.16 USDC to clear it.

### 4. `transfer_recipient` is disableable at creation

A `transferable: bool` flag alongside `cancellable` and `pausable`. A
compliance-bound sender — payroll, a KYC'd grant program — can pin the payee at
creation. Without it those senders simply could not use Fluxora.

---

## TTL, rent and archival

The hardest problem in the project, and the one existing implementations skip.

Persistent entries have a time-to-live in ledgers. When it runs out the entry is
archived and becomes unreadable until restored. A stream running twelve months
outlives its initial TTL. If a stream entry archives, the **tokens are not lost**
— they sit in the contract's pooled balance — but the accounting entry saying who
they belong to is inaccessible until someone pays to restore it.

Three mechanisms:

1. **Extend on every touch.** Every mutating call bumps that entry's TTL, so an
   actively-used stream never expires.
2. **Extend generously at creation**, targeting the stream's remaining lifetime
   plus a 30-day buffer, clamped to the network's `max_entry_ttl`. The clamp is
   not optional — a multi-year stream *will* need periodic extension regardless.
3. **Permissionless `extend_stream_ttl` and `batch_extend_ttl`.** Anyone can pay
   to keep any stream alive. Unauthenticated on purpose: a recipient's claim must
   never depend on the sender's continued goodwill. There is nothing to grief —
   the caller only ever *pays* rent, and TTL extension cannot move funds or
   change stream state.

Views deliberately do **not** extend TTL. They are called through simulation,
where a footprint write is at best noise. Keeping a stream alive is the explicit
job of `extend_stream_ttl`.

### What the tests prove, and what they cannot

The SDK's test host runs storage in recording mode, where reading an expired
persistent entry auto-restores it with data intact and TTL reset to
`min_persistent_entry_ttl`. That mirrors the on-network outcome of a
`RestoreFootprint` operation, so `test::ttl` genuinely proves:

- a year-long stream survives on keeper sweeps alone, with its accounting intact
  and paying out in full (`a_year_long_stream_survives_on_keeper_sweeps_alone`);
- an archived stream restores with every field of its accounting unchanged and
  the pool still fully backing it (`an_archived_stream_restores_with_its_accounting_intact`).

What unit tests **cannot** reproduce is the client-side dance on a real network,
where the transaction fails first and the caller must resubmit with a restore
footprint. That has no unit-test surface and belongs in the testnet exercise
(stage 4). The SDK should detect archived streams and surface a one-click restore
rather than an opaque error; `stream_exists` returning `false` for an id below
`stream_count` is the signal to key on.

---

## Function surface

```rust
// Lifecycle
create_stream(sender, recipient, token, deposit,
              start, end, cliff,
              cancellable, pausable, transferable) -> u64   // sender auth
top_up(stream_id, amount)                                   // sender auth
withdraw(stream_id, amount: Option<i128>) -> i128           // recipient auth; None = max
batch_withdraw(recipient, stream_ids) -> i128               // recipient auth
cancel(stream_id)                                           // sender auth
pause(stream_id) / resume(stream_id)                        // sender auth
transfer_recipient(stream_id, new_recipient)                // recipient auth

// Views (read-only, no TTL side effects)
get_stream(stream_id) -> Stream
withdrawable_of(stream_id) -> i128
vested_of(stream_id) -> i128
refundable_of(stream_id) -> i128
stream_count() -> u64
stream_exists(stream_id) -> bool

// Maintenance (permissionless)
extend_stream_ttl(stream_id) -> u32
batch_extend_ttl(stream_ids) -> u32
```

Both classic keypairs and custom `__check_auth` smart accounts work everywhere.

### Events

`stream_created`, `withdrawn`, `cancelled`, `paused`, `resumed`, `topped_up`,
`recipient_transferred`, `ttl_extended`.

Declared with `#[contractevent]`, so their schemas are embedded in the deployed
contract's interface spec — the indexer and TypeScript SDK generate typed
decoders from the contract itself rather than hand-rolling topic parsers.

Every event carries `stream_id` as a topic, plus the addresses an indexer routes
on, plus enough state to reconstruct the stream without replaying from genesis.
Field order and topic placement are ABI: adding a field is compatible,
reordering one is not.

---

## Repository layout

```
contracts/stream/
  src/
    lib.rs              contract entry points
    accrual.rs          pure vesting math, no Env
    storage.rs          storage access and TTL policy
    events.rs           event definitions
    types.rs            Stream, StreamStatus, DataKey
    error.rs            typed errors (discriminants are ABI)
    test/               140 tests, staged by build order
```

`accrual.rs` takes a `Stream` and a timestamp and returns a number — no `Env`, no
storage, no host calls. That keeps the interesting arithmetic in one auditable
place and makes the vesting model property-testable without a Soroban host, so a
case costs microseconds instead of a host invocation.

---

## Non-goals for v1

No admin key, no upgradeability, no global pause. No fee mechanism. No on-chain
stream discovery. No multi-token streams. No unlock curves other than cliff plus
linear. No cross-chain anything.

---

## Status

Stages 1–3 complete: contract core, full lifecycle, TTL and resource limits.

Next: testnet deploy with a CLI exercising every function against live testnet,
then the indexer, keeper and TypeScript SDK.

> **Note for deployment:** the `stellar` CLI must be at least version 27 to match
> the protocol. A protocol-23 CLI will scaffold and may misreport against a
> protocol-27 network.
