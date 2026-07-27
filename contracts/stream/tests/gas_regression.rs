// See docs/gas.md for the baseline update process and review bar.
use fluxora_stream::{
    CreateStreamParams, FluxoraStream, FluxoraStreamClient, PauseReason, StreamKind, WithdrawToParam,
};
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::{Client as TokenClient, StellarAssetClient},
    Address, Env,
};

// Per-invocation CPU budget (Soroban limit) with a 75% safety margin.
// The budget assertion fails if measured cost exceeds this threshold,
// guarding against inadvertent regressions (e.g. an increased MAX_PAGE_SIZE
// that worsens the O(n²) duplicate-ID scan).
const PER_INVOCATION_CPU_BUDGET: u64 = 25_000_000_000;

// Grace period (mirrors KEEPER_GRACE_PERIOD_SECONDS in lib.rs).
const KEEPER_GRACE: u64 = 604_800;

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

// Grace period (mirrors KEEPER_GRACE_PERIOD_SECONDS in lib.rs).
const KEEPER_GRACE: u64 = 604_800;

struct KeeperTestContext<'a> {
    env: Env,
    client: FluxoraStreamClient<'a>,
    sender: Address,
    recipient: Address,
    keeper: Address,
}

impl<'a> KeeperTestContext<'a> {
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
}

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

impl HasEnv for KeeperTestContext<'_> {
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

/// Gas regression baseline for `batch_withdraw_to`.
///
/// Uses a distinct destination address per withdrawal to exercise the
/// per-entry destination validation path alongside the O(n²) duplicate-ID
/// scan in `reject_duplicate_ids`.  The O(n²) scan costs roughly
/// n*(n-1)/2 comparisons at batch size n, so at MAX_PAGE_SIZE (100) the
/// worst case is ~4 950 element-by-element comparisons inside the helper.
#[test]
fn test_batch_withdraw_to_gas() {
    let sizes = [1, 10, 50, 100];

    for &size in &sizes {
        let ctx = TestContext::setup();

        let mut streams = soroban_sdk::Vec::new(&ctx.env);
        let mut destinations = soroban_sdk::Vec::new(&ctx.env);
        for _ in 0..size {
            streams.push_back(ctx.create_default_stream());
            destinations.push_back(Address::generate(&ctx.env));
        }

        let mut withdrawals = soroban_sdk::Vec::new(&ctx.env);
        for i in 0..size {
            withdrawals.push_back(WithdrawToParam {
                stream_id: streams.get(i as u32).unwrap(),
                destination: destinations.get(i as u32).unwrap(),
            });
        }

        ctx.env.ledger().set_timestamp(500); // Accrue tokens for all

        let cost = measure_gas(&ctx, |ctx| {
            ctx.client.batch_withdraw_to(&ctx.recipient, &withdrawals);
        });

        assert!(
            cost <= PER_INVOCATION_CPU_BUDGET,
            "batch_withdraw_to at size {} exceeded per-invocation CPU budget: {} > {}",
            size,
            cost,
            PER_INVOCATION_CPU_BUDGET,
        );

        println!("GAS_MEASUREMENT: batch_withdraw_to: {}: {}", size, cost);
    }
}

/// Gas regression baseline for `bulk_resume_streams_as_admin`.
///
/// Creates streams, pauses each one (advancing the ledger far enough to
/// clear the pause cooldown), then resumes them all in a single admin-authed
/// call.  The O(n²) duplicate-ID scan in `reject_duplicate_ids` dominates the
/// variable cost at large batch sizes.
#[test]
fn test_bulk_resume_streams_as_admin_gas() {
    let sizes = [1, 10, 50, 100];

    for &size in &sizes {
        let ctx = TestContext::setup();

        let mut streams = soroban_sdk::Vec::new(&ctx.env);
        for _ in 0..size {
            let id = ctx.create_default_stream();
            // Advance past the pause/resume cooldown (17 ledgers) so the
            // subsequent pause succeeds even if the ledger sequence is low.
            ctx.env.ledger().with_mut(|l| l.sequence_number += 32);
            ctx.client.pause_stream_as_admin(&id, &PauseReason::Administrative);
            streams.push_back(id);
        }

        let cost = measure_gas(&ctx, |ctx| {
            ctx.client.bulk_resume_streams_as_admin(&streams);
        });

        assert!(
            cost <= PER_INVOCATION_CPU_BUDGET,
            "bulk_resume_streams_as_admin at size {} exceeded per-invocation CPU budget: {} > {}",
            size,
            cost,
            PER_INVOCATION_CPU_BUDGET,
        );

        println!("GAS_MEASUREMENT: bulk_resume_streams_as_admin: {}: {}", size, cost);
    }
}

/// Gas regression baseline for `bulk_cancel_streams`.
///
/// Creates active streams owned by the sender then cancels them all in a
/// single call.  The O(n²) duplicate-ID scan in `reject_duplicate_ids`
/// contributes the variable-cost component that grows quadratically with
/// batch size.
#[test]
fn test_bulk_cancel_streams_gas() {
    let sizes = [1, 10, 50, 100];

    for &size in &sizes {
        let ctx = TestContext::setup();

        let mut streams = soroban_sdk::Vec::new(&ctx.env);
        for _ in 0..size {
            streams.push_back(ctx.create_default_stream());
        }

        ctx.env.ledger().set_timestamp(500); // Accrue tokens so cancellation is non-trivial

        let cost = measure_gas(&ctx, |ctx| {
            ctx.client.bulk_cancel_streams(&ctx.sender, &streams);
        });

        assert!(
            cost <= PER_INVOCATION_CPU_BUDGET,
            "bulk_cancel_streams at size {} exceeded per-invocation CPU budget: {} > {}",
            size,
            cost,
            PER_INVOCATION_CPU_BUDGET,
        );

        println!("GAS_MEASUREMENT: bulk_cancel_streams: {}: {}", size, cost);
    }
}

// ---------------------------------------------------------------------------
// keeper_cancel gas measurements
//
// Two variants capture the two meaningful cost paths:
//
//   partial_accrual — the common keeper incentive case: the stream expired with
//     an unstreamed balance, so the contract makes three token transfers
//     (recipient, sender, keeper).  This is the hot path for economically
//     rational keeper bots and the cost documented in docs/gas.md's
//     break-even formula.
//
//   fully_accrued   — the degenerate case: deposit == rate × duration, so
//     sender_refund_gross == 0, keeper_fee == 0 and no keeper transfer is
//     issued.  Only one token transfer (to the recipient) occurs.  Cost is
//     slightly lower than the partial_accrual variant.
//
// Both variants print a GAS_MEASUREMENT line that validate_gas.py picks up
// and compares against the JSON baseline in docs/gas.md.
// ---------------------------------------------------------------------------

/// keeper_cancel on a stream that still has an unstreamed balance (3 transfers).
///
/// Setup:
///   deposit = 10 000, rate = 5 token/s, start = 0, end = 1 000
///   → accrued at end_time = min(5 × 1 000, 10 000) = 5 000
///   → sender_refund_gross = 5 000
///   → keeper_fee = 5 000 × 50 / 10 000 = 25
///   → three token transfers: recipient 5 000, sender 4 975, keeper 25
#[test]
fn test_keeper_cancel_gas_partial_accrual() {
    let ctx = KeeperTestContext::setup();

    // Create the stream at t=0.
    ctx.env.ledger().set_timestamp(0);
    let stream_id = ctx.client.create_stream(
        &ctx.sender,
        &CreateStreamParams {
            recipient: ctx.recipient.clone(),
            deposit_amount: 10_000_i128,
            rate_per_second: 5_i128,
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

    // Advance past end_time + grace period so the stream is eligible.
    ctx.env.ledger().set_timestamp(1_000 + KEEPER_GRACE + 1);

    let cost = measure_gas(&ctx, |ctx| {
        ctx.client.keeper_cancel(&stream_id, &ctx.keeper);
    });

    // Print in the canonical GAS_MEASUREMENT format so validate_gas.py can
    // parse this line and compare it against the baseline in docs/gas.md.
    println!("GAS_MEASUREMENT: keeper_cancel: partial_accrual: {}", cost);
}

/// keeper_cancel on a stream that is fully accrued (1 transfer, keeper fee == 0).
///
/// Setup:
///   deposit = 1 000, rate = 1 token/s, start = 0, end = 1 000
///   → accrued at end_time = 1 000 == deposit
///   → sender_refund_gross = 0, keeper_fee = 0
///   → one token transfer: recipient 1 000; no sender or keeper transfers
#[test]
fn test_keeper_cancel_gas_fully_accrued() {
    let ctx = KeeperTestContext::setup();

    ctx.env.ledger().set_timestamp(0);
    let stream_id = ctx.client.create_stream(
        &ctx.sender,
        &CreateStreamParams {
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
        },
    );

    ctx.env.ledger().set_timestamp(1_000 + KEEPER_GRACE + 1);

    let cost = measure_gas(&ctx, |ctx| {
        ctx.client.keeper_cancel(&stream_id, &ctx.keeper);
    });

    println!("GAS_MEASUREMENT: keeper_cancel: fully_accrued: {}", cost);
}