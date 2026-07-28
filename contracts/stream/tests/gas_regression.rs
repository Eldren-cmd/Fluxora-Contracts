use fluxora_stream::{
    CreateStreamParams, FluxoraStream, FluxoraStreamClient, StreamKind,
};
use soroban_sdk::{token::Client as TokenClient, Address, Bytes, Env, Map};

struct TestContext<'a> {
    env: Env,
    client: FluxoraStreamClient<'a>,
    sender: Address,
    recipient: Address,
    keeper: Address,
}

impl<'a> TestContext<'a> {
    fn setup() -> Self {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, FluxoraStream);
        let client = FluxoraStreamClient::new(&env, &contract_id);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = StellarAssetClient::new(&env, &token_id);

        let admin = Address::generate(&env);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let keeper = Address::generate(&env);

        client.init(&token_id, &admin);

        // Fund the sender using the admin's minting power
        sac.mint(&sender, &1_000_000_i128);
        // Provide default allowance so create_stream can pull the deposit.
        TokenClient::new(&env, &token_id).approve(&sender, &contract_id, &i128::MAX, &100_000);

        Self {
            env,
            client,
            sender,
            recipient,
            keeper,
        }
    }

    fn create_default_stream(&self) -> u64 {
        let amount = 1000_i128;
        let rate = 1_i128;
        let start_time = 0u64;
        let cliff_time = 0u64;
        let end_time = 1000u64;

        self.client.create_stream(
            &self.sender,
            &CreateStreamParams {
                recipient: self.recipient.clone(),
                deposit_amount: amount,
                rate_per_second: rate,
                start_time: start_time,
                cliff_time: cliff_time,
                end_time: end_time,
                withdraw_dust_threshold: Some(0),
                memo: None,
                metadata: None,
                kind: StreamKind::Linear,
                irrevocable: None,
                witness: None,
            },
        )
    }
}

// KeeperTestContext is an alias for TestContext — same setup, same fields.
// Keeper tests use the identical harness; the type alias keeps the test
// names readable without duplicating the setup code.
type KeeperTestContext<'a> = TestContext<'a>;

fn measure_gas<F, C>(ctx: &C, f: F) -> u64
where
    F: FnOnce(&C),
    C: HasEnv,
{
    ctx.env().budget().reset_unlimited();
    f(ctx);
    ctx.env().budget().cpu_instruction_cost()
}

trait HasEnv {
    fn env(&self) -> &Env;
}

impl HasEnv for TestContext<'_> {
    fn env(&self) -> &Env {
        &self.env
    }
}

#[test]
fn test_create_stream_gas() {
    let ctx = TestContext::setup();

    let cost = measure_gas(&ctx, |ctx| {
        ctx.create_default_stream();
    });

    println!("GAS_MEASUREMENT: create_stream: single: {}", cost);
}

#[test]
fn test_withdraw_gas() {
    let ctx = TestContext::setup();

    let stream_id = ctx.create_default_stream();
    ctx.env.ledger().set_timestamp(500); // Accrue 500 tokens

    let cost = measure_gas(&ctx, |ctx| {
        ctx.client.withdraw(&stream_id);
    });

    println!("GAS_MEASUREMENT: withdraw: single: {}", cost);
}

#[test]
fn test_batch_withdraw_gas() {
    let sizes = [1, 10, 50, 100];

    for &size in &sizes {
        let ctx = TestContext::setup();

        let mut streams = soroban_sdk::Vec::new(&ctx.env);
        for _ in 0..size {
            streams.push_back(ctx.create_default_stream());
        }

        ctx.env.ledger().set_timestamp(500); // Accrue tokens for all

        let cost = measure_gas(&ctx, |ctx| {
            ctx.client.batch_withdraw(&ctx.recipient, &streams);
        });

        assert!(
            cost <= PER_INVOCATION_CPU_BUDGET,
            "batch_withdraw at size {} exceeded per-invocation CPU budget: {} > {}",
            size,
            cost,
            PER_INVOCATION_CPU_BUDGET,
        );

        println!("GAS_MEASUREMENT: batch_withdraw: {}: {}", size, cost);
    }
}

// ---------------------------------------------------------------------------
// Gas regression: metadata operations
// ---------------------------------------------------------------------------

/// Helper: build a metadata map with `count` entries "k0"→"v0", … "kN"→"vN".
fn metadata_n(env: &Env, count: u32) -> Map<Bytes, Bytes> {
    let mut m: Map<Bytes, Bytes> = Map::new(env);
    for i in 0..count {
        let k = Bytes::from_slice(env, format!("k{}", i).as_bytes());
        let v = Bytes::from_slice(env, format!("v{}", i).as_bytes());
        m.set(k, v);
    }
    m
}

/// Measure gas for `create_streams_partial` with a single entry carrying metadata.
#[test]
fn test_create_stream_with_metadata_gas() {
    let ctx = TestContext::setup();

    // Full metadata: MAX_METADATA_KEYS × (32-byte key + 128-byte value) at max aggregate.
    let meta = metadata_n(&ctx.env, fluxora_stream::MAX_METADATA_KEYS);
    let recipient = Address::generate(&ctx.env);
    let params = soroban_sdk::vec![
        &ctx.env,
        CreateStreamParams {
            recipient,
            deposit_amount: 1000,
            rate_per_second: 1,
            start_time: 0,
            cliff_time: 0,
            end_time: 1000,
            withdraw_dust_threshold: None,
            memo: None,
            kind: StreamKind::Linear,
            metadata: Some(meta),
        },
    ];

    let cost = measure_gas(&ctx, |ctx| {
        ctx.client
            .create_streams_partial(&ctx.sender, &params);
    });

    println!("GAS_MEASUREMENT: create_stream_with_metadata: full: {}", cost);

    // Also measure without metadata for comparison
    let recipient2 = Address::generate(&ctx.env);
    let params_no_meta = soroban_sdk::vec![
        &ctx.env,
        CreateStreamParams {
            recipient: recipient2,
            deposit_amount: 1000,
            rate_per_second: 1,
            start_time: 0,
            cliff_time: 0,
            end_time: 1000,
            withdraw_dust_threshold: None,
            memo: None,
            kind: StreamKind::Linear,
            metadata: None,
        },
    ];

    let cost_no_meta = measure_gas(&ctx, |ctx| {
        ctx.client
            .create_streams_partial(&ctx.sender, &params_no_meta);
    });

    println!(
        "GAS_MEASUREMENT: create_stream_without_metadata: baseline: {}",
        cost_no_meta
    );
}

/// Measure gas for `get_stream_metadata` on a stream with full metadata.
#[test]
fn test_get_stream_metadata_gas() {
    let ctx = TestContext::setup();

    // Create a stream with metadata so we can read it back
    let meta = metadata_n(&ctx.env, fluxora_stream::MAX_METADATA_KEYS);
    let recipient = Address::generate(&ctx.env);
    let params = soroban_sdk::vec![
        &ctx.env,
        CreateStreamParams {
            recipient,
            deposit_amount: 1000,
            rate_per_second: 1,
            start_time: 0,
            cliff_time: 0,
            end_time: 1000,
            withdraw_dust_threshold: None,
            memo: None,
            kind: StreamKind::Linear,
            metadata: Some(meta),
        },
    ];
    let results = ctx
        .client
        .create_streams_partial(&ctx.sender, &params);
    let stream_id = results.get(0).unwrap().stream_id.unwrap();

    let cost = measure_gas(&ctx, |ctx| {
        ctx.client.get_stream_metadata(&stream_id);
    });

    println!(
        "GAS_MEASUREMENT: get_stream_metadata: full: {}",
        cost
    );
}

// ---------------------------------------------------------------------------
// Additional edge-case gas measurements (issue #1286)
//
// These tests fill the remaining coverage gaps identified in the issue review:
//
//   cancel_stream_single         — single `cancel_stream` by the sender on a
//                                  partially-accrued active stream.  The bulk
//                                  variant (`bulk_cancel_streams`) is already
//                                  measured but does not expose the per-stream
//                                  cost in isolation.
//
//   zero_accrual_withdraw        — `withdraw` when the cliff has not yet been
//                                  reached.  No token transfer is issued; the
//                                  accrual short-circuit path is exercised and
//                                  its cost documented.
//
//   update_rate_per_second       — `update_rate_per_second` (rate increase) on
//                                  an active stream.  Checkpoints the accrual,
//                                  validates the new rate against the max-rate
//                                  cap, then saves the updated stream.
//
//   decrease_rate_per_second     — `decrease_rate_per_second` on an active
//                                  stream.  Checkpoints accrual, computes a
//                                  partial refund, persists state, and issues
//                                  a token transfer back to the sender.
//
//   top_up_stream                — `top_up_stream` adds deposit to an active
//                                  stream.  Pulls tokens from the funder and
//                                  increases the global liabilities counter.
//
//   shorten_stream_end_time      — `shorten_stream_end_time` truncates an active
//                                  stream's schedule, computes a sender refund,
//                                  and issues the refund token transfer.
//
//   extend_stream_end_time       — `extend_stream_end_time` pushes an active
//                                  stream's end time further into the future when
//                                  the existing deposit is sufficient to cover the
//                                  extended schedule at the current rate.
//
//   emergency_pause_create       — `create_stream` attempted while the contract
//                                  is under emergency pause.  The call should
//                                  revert with `GloballyPaused` after a minimal
//                                  storage read; this test documents the cost of
//                                  the early-exit guard.
// ---------------------------------------------------------------------------

/// Gas baseline for `cancel_stream` (single stream, partial accrual).
///
/// Measures the cost of a sender-initiated cancellation at mid-stream (t=500
/// on a 0→1 000 schedule).  The contract executes:
///   1. Load stream.
///   2. Calculate accrued-to-date.
///   3. Transfer accrued portion to recipient.
///   4. Transfer unstreamed refund to sender.
///   5. Persist `Cancelled` state with `cancelled_at`.
///
/// Two token transfers occur (recipient + sender), so this is more expensive
/// than a plain `withdraw` (one transfer) and cheaper than `keeper_cancel`
/// (three transfers plus fee arithmetic).
///
/// Setup: 1 000-token linear stream, t=500 → 500 tokens accrued, 500 refunded.
#[test]
fn test_cancel_stream_single_gas() {
    let ctx = TestContext::setup();

    let stream_id = ctx.create_default_stream();

    // Advance to half-way so there is meaningful accrual AND a meaningful refund.
    ctx.env.ledger().set_timestamp(500);

    let cost = measure_gas(&ctx, |ctx| {
        ctx.client.cancel_stream(&stream_id);
    });

    assert!(
        cost <= PER_INVOCATION_CPU_BUDGET,
        "cancel_stream (single) exceeded per-invocation CPU budget: {} > {}",
        cost,
        PER_INVOCATION_CPU_BUDGET,
    );

    println!("GAS_MEASUREMENT: cancel_stream: single: {}", cost);
}

/// Gas baseline for `withdraw` when the stream's cliff has not yet been reached.
///
/// Before `cliff_time`, `calculate_accrued_amount` returns 0.  The `withdraw`
/// implementation detects a zero withdrawable balance, skips all token-transfer
/// and state-mutation work, and returns 0 immediately.  This test documents
/// the cost of that short-circuit path.
///
/// The test is labelled `zero_accrual` (not `before_cliff`) because the same
/// zero-withdrawable path is also hit when the stream is already fully drained
/// and the caller invokes `withdraw` again — cliff semantics are just the most
/// natural way to set up the pre-condition in isolation.
///
/// Setup: 1 000-token stream with cliff at t=500; ledger is at t=100
///        → 0 tokens accrued, no transfer issued.
#[test]
fn test_withdraw_zero_accrual_gas() {
    let ctx = TestContext::setup();

    // Create a stream where the cliff is far in the future relative to the
    // test's ledger timestamp (t=0 initially).
    let stream_id = ctx.client.create_stream(
        &ctx.sender,
        &CreateStreamParams {
            recipient: ctx.recipient.clone(),
            deposit_amount: 1_000_i128,
            rate_per_second: 1_i128,
            start_time: 0u64,
            cliff_time: 500u64, // cliff well in the future
            end_time: 1_000u64,
            withdraw_dust_threshold: Some(0_i128),
            memo: None,
            metadata: None,
            kind: StreamKind::Linear,
            irrevocable: None,
            witness: None,
        },
    );

    // Advance to before the cliff — accrual is 0.
    ctx.env.ledger().set_timestamp(100);

    let cost = measure_gas(&ctx, |ctx| {
        ctx.client.withdraw(&stream_id);
    });

    assert!(
        cost <= PER_INVOCATION_CPU_BUDGET,
        "withdraw (zero_accrual / pre-cliff) exceeded per-invocation CPU budget: {} > {}",
        cost,
        PER_INVOCATION_CPU_BUDGET,
    );

    println!("GAS_MEASUREMENT: withdraw_zero_accrual: single: {}", cost);
}

/// Gas baseline for `update_rate_per_second` (rate increase, active stream).
///
/// `update_rate_per_second` checkpoints the current accrual, validates the
/// new rate against the governance-controlled cap and deposit ceiling, and
/// saves the updated stream.  No token transfer occurs (deposit already locked).
///
/// Setup:
///   deposit = 2 000, rate = 1/s, start = 0, end = 1 000.
///   At t=300 we increase the rate to 2/s.
///   new_total_streamable = 2 × 1 000 = 2 000 ≤ deposit, so the update is valid.
#[test]
fn test_update_rate_per_second_gas() {
    let ctx = TestContext::setup();

    // Deposit must cover the higher rate for the full duration:
    //   rate=2, duration=1000 → total_streamable=2000 ≤ deposit=2000 ✓
    let stream_id = ctx.client.create_stream(
        &ctx.sender,
        &CreateStreamParams {
            recipient: ctx.recipient.clone(),
            deposit_amount: 2_000_i128,
            rate_per_second: 1_i128,  // start at rate=1
            start_time: 0u64,
            cliff_time: 0u64,
            end_time: 1_000u64,
            withdraw_dust_threshold: Some(0_i128),
            memo: None,
            metadata: None,
            kind: StreamKind::Linear,
            irrevocable: None,
            witness: None,
        },
    );

    // Advance ledger sequence past the rate-change cooldown before the call.
    ctx.env.ledger().with_mut(|l| {
        l.timestamp = 300;
        l.sequence_number += 32; // clear rate-change cooldown
    });

    let cost = measure_gas(&ctx, |ctx| {
        ctx.client.update_rate_per_second(&stream_id, &2_i128);
    });

    assert!(
        cost <= PER_INVOCATION_CPU_BUDGET,
        "update_rate_per_second exceeded per-invocation CPU budget: {} > {}",
        cost,
        PER_INVOCATION_CPU_BUDGET,
    );

    println!("GAS_MEASUREMENT: update_rate_per_second: single: {}", cost);
}

/// Gas baseline for `decrease_rate_per_second` (rate decrease + refund).
///
/// `decrease_rate_per_second` checkpoints the current accrual, recomputes the
/// new deposit ceiling under the lower rate, computes the sender refund, persists
/// the updated state (CEI order), and issues one token transfer back to the sender.
///
/// Setup:
///   deposit = 2 000, rate = 2/s, start = 0, end = 1 000.
///   At t=300 we decrease rate to 1/s.
///   Accrued-to-date = 300 × 2 = 600.  Remaining seconds = 700.
///   Future accrual at new rate = 1 × 700 = 700.  New deposit = 600 + 700 = 1 300.
///   Refund = 2 000 − 1 300 = 700 tokens transferred back to sender.
#[test]
fn test_decrease_rate_per_second_gas() {
    let ctx = TestContext::setup();

    // Create a stream where deposit covers rate=2 for the full duration so the
    // decrease to rate=1 produces a meaningful refund.
    let stream_id = ctx.client.create_stream(
        &ctx.sender,
        &CreateStreamParams {
            recipient: ctx.recipient.clone(),
            deposit_amount: 2_000_i128,  // covers rate=2 × duration=1000
            rate_per_second: 2_i128,
            start_time: 0u64,
            cliff_time: 0u64,
            end_time: 1_000u64,
            withdraw_dust_threshold: Some(0_i128),
            memo: None,
            metadata: None,
            kind: StreamKind::Linear,
            irrevocable: None,
            witness: None,
        },
    );

    ctx.env.ledger().with_mut(|l| {
        l.timestamp = 300;
        l.sequence_number += 32; // clear rate-change cooldown
    });

    let cost = measure_gas(&ctx, |ctx| {
        ctx.client.decrease_rate_per_second(&stream_id, &1_i128);
    });

    assert!(
        cost <= PER_INVOCATION_CPU_BUDGET,
        "decrease_rate_per_second exceeded per-invocation CPU budget: {} > {}",
        cost,
        PER_INVOCATION_CPU_BUDGET,
    );

    println!("GAS_MEASUREMENT: decrease_rate_per_second: single: {}", cost);
}

/// Gas baseline for `top_up_stream` (add deposit to an active stream).
///
/// `top_up_stream` pulls tokens from the funder, increases the stream's
/// deposit_amount, and updates the global TotalLiabilities counter.  No
/// schedule change occurs.
///
/// Setup: 1 000-token stream active at t=300; top-up amount = 500 tokens.
#[test]
fn test_top_up_stream_gas() {
    let ctx = TestContext::setup();

    let stream_id = ctx.create_default_stream();

    // Advance the ledger so the stream is live (past start) but not yet expired.
    ctx.env.ledger().set_timestamp(300);

    // The top-up funder can be the sender (the default sender already has
    // a large allowance set up by TestContext::setup).
    let cost = measure_gas(&ctx, |ctx| {
        ctx.client.top_up_stream(&stream_id, &ctx.sender, &500_i128);
    });

    assert!(
        cost <= PER_INVOCATION_CPU_BUDGET,
        "top_up_stream exceeded per-invocation CPU budget: {} > {}",
        cost,
        PER_INVOCATION_CPU_BUDGET,
    );

    println!("GAS_MEASUREMENT: top_up_stream: single: {}", cost);
}

/// Gas baseline for `shorten_stream_end_time` (schedule truncation + refund).
///
/// `shorten_stream_end_time` checkpoints accrual, computes a sender refund for
/// the truncated portion, persists the updated schedule (CEI), and issues one
/// token transfer back to the sender.
///
/// Setup: 1 000-token stream (rate=1/s, 0→1 000); at t=300 we shorten to t=600.
///   Remaining seconds at t=600 = 600 − 0 = 600.  New max streamable = 600.
///   Accrued-to-date = 300 ≤ 600, so new_deposit = 600.
///   Refund = 1 000 − 600 = 400 tokens transferred to sender.
#[test]
fn test_shorten_stream_end_time_gas() {
    let ctx = TestContext::setup();

    let stream_id = ctx.create_default_stream();

    // Advance to t=300 so accrual is meaningful and refund is non-zero.
    ctx.env.ledger().set_timestamp(300);

    let cost = measure_gas(&ctx, |ctx| {
        ctx.client.shorten_stream_end_time(&stream_id, &600u64);
    });

    assert!(
        cost <= PER_INVOCATION_CPU_BUDGET,
        "shorten_stream_end_time exceeded per-invocation CPU budget: {} > {}",
        cost,
        PER_INVOCATION_CPU_BUDGET,
    );

    println!("GAS_MEASUREMENT: shorten_stream_end_time: single: {}", cost);
}

/// Gas baseline for `extend_stream_end_time` (schedule extension, no token transfer).
///
/// `extend_stream_end_time` moves the stream's end time forward without changing
/// the rate or deposit.  The existing deposit must be sufficient to cover the
/// extended schedule at the current rate.  No token transfer occurs.
///
/// Setup: 2 000-token stream (rate=1/s, 0→1 000).  At t=300 we extend to t=1 500.
///   new_total_streamable = 1 × 1 500 = 1 500 ≤ deposit=2 000 ✓ (no extra transfer).
#[test]
fn test_extend_stream_end_time_gas() {
    let ctx = TestContext::setup();

    // Use a stream with extra deposit so the extended schedule still fits.
    let stream_id = ctx.client.create_stream(
        &ctx.sender,
        &CreateStreamParams {
            recipient: ctx.recipient.clone(),
            deposit_amount: 2_000_i128,  // covers rate=1 × end=1500
            rate_per_second: 1_i128,
            start_time: 0u64,
            cliff_time: 0u64,
            end_time: 1_000u64,
            withdraw_dust_threshold: Some(0_i128),
            memo: None,
            metadata: None,
            kind: StreamKind::Linear,
            irrevocable: None,
            witness: None,
        },
    );

    // Advance to mid-stream.
    ctx.env.ledger().set_timestamp(300);

    let cost = measure_gas(&ctx, |ctx| {
        ctx.client.extend_stream_end_time(&stream_id, &1_500u64);
    });

    assert!(
        cost <= PER_INVOCATION_CPU_BUDGET,
        "extend_stream_end_time exceeded per-invocation CPU budget: {} > {}",
        cost,
        PER_INVOCATION_CPU_BUDGET,
    );

    println!("GAS_MEASUREMENT: extend_stream_end_time: single: {}", cost);
}

/// Gas baseline for the emergency-pause guard on `create_stream`.
///
/// When `set_global_emergency_paused(true)` is active, every state-mutating
/// entry point (including `create_stream`) calls `require_not_globally_paused`
/// early in its execution.  That function reads one instance-storage key
/// (`GlobalEmergencyPaused`) and returns `GloballyPaused` before any stream
/// validation, deposit transfer, or storage write occurs.
///
/// This test captures the cost of that early-exit guard so that any future
/// change to the pause-check overhead (e.g. additional flag reads) is detected.
/// The test calls `try_create_stream` (the fallible variant) so the panic from
/// the expected error does not abort the test process.
///
/// Setup: emergency pause activated; `create_stream` called with a valid payload
///        → call reverts at `require_not_globally_paused`.
#[test]
fn test_create_stream_under_emergency_pause_gas() {
    let ctx = TestContext::setup();

    // Activate the global emergency pause.
    ctx.client.set_global_emergency_paused(&true);

    let params = CreateStreamParams {
        recipient: ctx.recipient.clone(),
        deposit_amount: 1_000_i128,
        rate_per_second: 1_i128,
        start_time: 0u64,
        cliff_time: 0u64,
        end_time: 1_000u64,
        withdraw_dust_threshold: Some(0_i128),
        memo: None,
        metadata: None,
        kind: StreamKind::Linear,
        irrevocable: None,
        witness: None,
    };

    // Reset budget and call the fallible variant so we capture cost even on revert.
    ctx.env.budget().reset_unlimited();
    let _result = ctx.client.try_create_stream(&ctx.sender, &params);
    let cost = ctx.env.budget().cpu_instruction_cost();

    assert!(
        cost <= PER_INVOCATION_CPU_BUDGET,
        "create_stream (emergency_pause guard) exceeded per-invocation CPU budget: {} > {}",
        cost,
        PER_INVOCATION_CPU_BUDGET,
    );

    println!(
        "GAS_MEASUREMENT: create_stream_emergency_pause: single: {}",
        cost
    );
}
