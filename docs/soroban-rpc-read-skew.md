# Soroban RPC read-skew: your reads can go backwards in time

*A note from the Fluxora team. First observed 2026-08-12 against Stellar
testnet, protocol 27, `stellar-cli` 27.1.0.*

## The short version

The public Soroban RPC endpoints are load-balanced across multiple nodes that
are **not at the same ledger height**. Two consecutive requests can be served by
different backends, so the ledger your read observes can move *backwards*.

If you write and then immediately read, you can observe pre-write state. If you
combine two reads into one derived figure, the two halves can come from
different ledgers and the figure can be arithmetically impossible.

Neither failure produces an error. You get a plausible wrong answer.

## How we hit it

Fluxora is a payment-streaming contract. Value accrues continuously, so we have
a conservation invariant:

```
vested(t) + refundable(t) == deposited      exactly, at every instant
```

Our on-chain test suite proves this per-ledger over thousands of random
schedules. So when a script that calls the two views back to back against live
testnet reported

```
vested=22000000  refundable=583000000  deposited=600000000
  ->  22000000 + 583000000 = 605000000   ✗ off by 5000000
```

the natural conclusion was a contract bug — 5,000,000 stroops conjured from
nowhere.

It was not. On that stream the rate was 1,000,000 stroops/second, so the
discrepancy was **exactly five seconds of accrual**. The two view calls had
landed on ledgers about five apart. `vested` was read from a *newer* ledger than
`refundable`, so the pair described two different moments and their sum was
meaningless.

The same effect made a `top_up` look like a no-op: the write succeeded and was
confirmed, but the `get_stream` immediately after was served by a lagging
backend and returned the pre-top-up deposit. We spent real time hunting a
contract bug that did not exist.

## Reproduction

`getLatestLedger` is enough — no contract required.

```bash
prev=0
for i in $(seq 1 25); do
  s=$(curl -s -X POST https://soroban-testnet.stellar.org \
        -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","id":1,"method":"getLatestLedger"}' \
      | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"]["sequence"])')
  if [ "$prev" -ne 0 ] && [ "$s" -lt "$prev" ]; then
    echo "went BACKWARDS: $prev -> $s"
  fi
  prev=$s
done
```

Our first run, around ledger 4,097,07x:

```
went BACKWARDS: 4097075 -> 4097071
went BACKWARDS: 4097075 -> 4097071
went BACKWARDS: 4097076 -> 4097071
went BACKWARDS: 4097076 -> 4097071
went BACKWARDS: 4097076 -> 4097071
went BACKWARDS: 4097076 -> 4097071
backwards transitions in 25 reads: 6
```

Six in twenty-five, with a spread of about five ledgers — roughly **25 seconds
of apparent time travel**.

## It is intermittent, and that is the important part

**We could not reproduce it on demand.** Everything we tried afterwards was
clean:

| run | method | samples | backwards steps |
|---|---|---|---|
| 1 | `curl`, fresh connection, no delay | 25 | **6** |
| 2 | Python `urllib`, 150 ms interval | 60 | 0 |
| 3 | `curl`, fresh connection, no delay | 60 | 0 |
| 4 | Python `urllib`, 200 ms interval, 8 minutes | 375 | 0 |

495 subsequent samples, zero regressions, heights forming a clean monotonic
staircase — including run 3, which repeated run 1's method exactly. Runs 2–4
were taken roughly 30 minutes after run 1, around ledger 4,103,9xx versus
4,097,07x.

We are publishing the positive observation anyway, and stating the negative
evidence beside it, because "we saw this once and then could not make it happen
again" is the honest description and is more useful to you than a confident
claim we cannot support.

So this is not a constant property of the endpoint. It appears to be episodic —
most plausibly a backend that had fallen behind and was catching up, in rotation
for a window of minutes. Which backends are in rotation and how far they have
drifted will vary by time, by region, and by what the operators are doing.

That is exactly what makes it dangerous:

- It will not show up in your local testing.
- It will not show up in CI.
- It will show up for some fraction of your users, some of the time, as numbers
  that do not add up.
- It is intermittent enough that you will suspect your own contract first. We
  did.

Do not treat "I ran the loop and saw nothing" as evidence that your client is
safe. Design for it.

## What to do about it

### 1. Put a barrier after every write

Do not read straight after a write. Wait until the backends you can see have
caught up to the ledger containing your transaction. A cheap version: sample
`getLatestLedger` until several consecutive samples are all at or beyond your
target height.

```bash
settle() {
  local target hi=0 ok=0 tries=0 s
  target=$(latest_ledger)
  while [ "$ok" -lt 4 ] && [ "$tries" -lt 40 ]; do
    s=$(latest_ledger)
    [ "$s" -gt "$hi" ] && hi=$s
    if [ "$s" -ge "$target" ] && [ "$s" -gt 0 ]; then ok=$((ok+1)); else ok=0; fi
    tries=$((tries+1)); sleep 1
  done
}
```

Requiring several *consecutive* samples at or above the target is what makes
this work: a single passing sample only tells you that *one* backend has caught
up, and the next request may well go to a different one.

### 2. Pin multi-value reads to one ledger

If you derive a figure from more than one call, the calls must observe the same
ledger, or the figure is unsound. Options, best first:

- **Return everything you need from a single call.** Our `get_stream` returns
  the full struct, so a client can compute vested, withdrawable and refundable
  itself from one atomic read. This is the real fix, and it is worth designing
  your contract's views around.
- **Read `latestLedger` from each response** — every Soroban RPC response
  carries it — and discard the set if the values disagree, then retry.
- **Tolerate a bounded skew** if you are only sanity-checking rather than
  displaying. Our exercise script allows a window of about 30 seconds of accrual
  on the conservation check for exactly this reason, and says so in a comment.

### 3. Never assert exact equality across two calls in a test

A test that asserts `a() + b() == c()` across three separate simulations against
a public endpoint will fail intermittently, and every failure will look like a
contract bug. Either derive all three from one call, or assert within a
tolerance you can justify.

### 4. Run your own RPC if you need read-after-write

A single node you control cannot skew against itself. For an indexer or a keeper
this is worth the operational cost; the barrier pattern above is a mitigation,
not a guarantee.

## What this is not

This is **not** a consensus problem and not a data-integrity problem. Every node
is serving a valid, internally consistent view of a real ledger. Ledger *N* is
not wrong; it is just older than ledger *N+5*, and you were not expecting to be
handed the older one after having already seen the newer one.

It is also not specific to Soroban's design — it is the ordinary read-your-writes
consistency problem that any load-balanced read replica has. It is worth writing
down only because the Soroban client tooling presents a single URL as though it
were a single node, nothing in the API surface hints that consecutive calls may
regress, and the failure mode looks so much like a contract bug that you will
debug the wrong thing first.

## Summary for client authors

| do | don't |
|---|---|
| barrier after writes, requiring several consecutive samples | read immediately after a write |
| derive everything from one call where possible | combine two view calls into one number |
| check `latestLedger` on each response and discard mismatched sets | assume consecutive reads are monotonic |
| assert within a justified tolerance in tests | assert exact cross-call equality against a public endpoint |
| run your own node if you need read-after-write | assume "I couldn't reproduce it" means it won't happen |

---

*Found while building [Fluxora](https://github.com/Fluxora-Org/Fluxora-Contracts),
a continuous payment streaming primitive for Soroban. Corrections welcome — if
you have data on how the spread varies by region or over time, we would like to
see it.*
