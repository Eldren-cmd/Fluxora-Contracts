//! Stage 1 — `create_stream`: happy path, custody, and every validation gate.

use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::Address;

use super::common::*;
use crate::{storage, Error, StreamStatus};

#[test]
fn create_moves_deposit_into_the_pool() {
    let h = Harness::new();
    let before = h.balance(&h.sender);

    let id = h.create_simple(1_000 * ONE, 100 * DAY);

    assert_eq!(id, 0, "first stream must be id 0");
    assert_eq!(h.balance(&h.sender), before - 1_000 * ONE);
    assert_eq!(h.pool(), 1_000 * ONE);
    h.assert_pool_exact();
}

#[test]
fn create_records_every_field() {
    let h = Harness::new();
    let start = h.now() + DAY;
    let end = start + 100 * DAY;
    let cliff = start + 10 * DAY;

    let id = h.create(500 * ONE, start, end, cliff, true, false, true);
    let s = h.get(id);

    assert_eq!(s.sender, h.sender);
    assert_eq!(s.recipient, h.recipient);
    assert_eq!(s.token, h.token);
    assert_eq!(s.deposited, 500 * ONE);
    assert_eq!(s.withdrawn, 0);
    assert_eq!(s.start_time, start);
    assert_eq!(s.end_time, end);
    assert_eq!(s.cliff_time, cliff);
    assert!(s.cancellable);
    assert!(!s.pausable);
    assert!(s.transferable);
    assert_eq!(s.paused_at, None);
    assert_eq!(s.paused_total, 0);
    assert_eq!(s.status, StreamStatus::Active);
}

#[test]
fn stream_ids_are_monotonic_and_never_reused() {
    let h = Harness::new();
    for expected in 0..5u64 {
        assert_eq!(h.create_simple(10 * ONE, DAY), expected);
    }
    assert_eq!(h.client.stream_count(), 5);
}

#[test]
fn multiple_streams_pool_together_without_interference() {
    let h = Harness::new();
    let a = h.create_simple(100 * ONE, 100 * DAY);
    let b = h.create_simple(300 * ONE, 50 * DAY);

    assert_eq!(h.pool(), 400 * ONE);
    h.assert_pool_exact();

    h.advance(25 * DAY);

    // Different rates, independent accrual: a is 25% through, b is 50%.
    assert_eq!(h.client.vested_of(&a), 25 * ONE);
    assert_eq!(h.client.vested_of(&b), 150 * ONE);
}

// --- Validation -----------------------------------------------------------

#[test]
fn rejects_stream_to_self() {
    let h = Harness::new();
    let start = h.now();
    let err = h
        .client
        .try_create_stream(
            &h.sender,
            &h.sender,
            &h.token,
            &(100 * ONE),
            &start,
            &(start + DAY),
            &start,
            &true,
            &true,
            &true,
        )
        .unwrap_err()
        .unwrap();
    assert_eq!(err, Error::SelfStream);
}

#[test]
fn rejects_non_positive_deposit() {
    let h = Harness::new();
    for deposit in [0i128, -1, -1_000 * ONE] {
        let start = h.now();
        let err = h
            .client
            .try_create_stream(
                &h.sender,
                &h.recipient,
                &h.token,
                &deposit,
                &start,
                &(start + DAY),
                &start,
                &true,
                &true,
                &true,
            )
            .unwrap_err()
            .unwrap();
        assert_eq!(err, Error::InvalidDeposit, "deposit {deposit}");
    }
}

/// `end_time <= start_time` would make `duration` zero and divide by zero in
/// the accrual math. It must never reach storage.
#[test]
fn rejects_non_positive_duration() {
    let h = Harness::new();
    let start = h.now();
    for end in [start, start - 1, start - DAY] {
        let err = h
            .client
            .try_create_stream(
                &h.sender,
                &h.recipient,
                &h.token,
                &(100 * ONE),
                &start,
                &end,
                &start,
                &true,
                &true,
                &true,
            )
            .unwrap_err()
            .unwrap();
        assert_eq!(err, Error::InvalidTimeRange, "end {end}");
    }
}

#[test]
fn rejects_cliff_outside_the_schedule() {
    let h = Harness::new();
    let start = h.now();
    let end = start + 100 * DAY;

    for cliff in [start - 1, end + 1, end + DAY] {
        let err = h
            .client
            .try_create_stream(
                &h.sender,
                &h.recipient,
                &h.token,
                &(100 * ONE),
                &start,
                &end,
                &cliff,
                &true,
                &true,
                &true,
            )
            .unwrap_err()
            .unwrap();
        assert_eq!(err, Error::InvalidCliff, "cliff {cliff}");
    }

    // Both endpoints are legal: cliff == start means no cliff, cliff == end
    // means a single lump sum at maturity.
    h.create(100 * ONE, start, end, start, true, true, true);
    h.create(100 * ONE, start, end, end, true, true, true);
}

/// The dust-rate footgun: a treasury streaming a small grant over a year would
/// otherwise create a stream whose per-second rate truncates to zero.
#[test]
fn rejects_deposit_below_one_stroop_per_second() {
    let h = Harness::new();
    let start = h.now();
    let end = start + YEAR;
    let duration = YEAR as i128;

    let err = h
        .client
        .try_create_stream(
            &h.sender,
            &h.recipient,
            &h.token,
            &(duration - 1),
            &start,
            &end,
            &start,
            &true,
            &true,
            &true,
        )
        .unwrap_err()
        .unwrap();
    assert_eq!(err, Error::DepositRateTooLow);

    // Exactly one stroop per second is the boundary, and it is allowed.
    h.create(duration, start, end, start, true, true, true);
}

/// A year-long USDC stream needs only ~3.16 USDC to clear the rate floor, so
/// the check excludes nothing anyone would realistically create.
#[test]
fn rate_floor_does_not_block_realistic_grants() {
    let h = Harness::new();
    let start = h.now();
    // 4 USDC over a year, in stroops: 40_000_000 > 31_536_000 seconds.
    h.create(4 * ONE, start, start + YEAR, start, true, true, true);
}

#[test]
fn rejects_deposit_that_would_overflow_accrual() {
    let h = Harness::new();
    let start = h.now();
    let end = start + YEAR;

    // deposit * duration must fit in an i128. Proving it here means the
    // multiplication inside `vested` can never overflow later.
    let err = h
        .client
        .try_create_stream(
            &h.sender,
            &h.recipient,
            &h.token,
            &(i128::MAX / 1_000),
            &start,
            &end,
            &start,
            &true,
            &true,
            &true,
        )
        .unwrap_err()
        .unwrap();
    assert_eq!(err, Error::Overflow);
}

// --- Timing policy: clock skew and the TTL horizon --------------------------
//
// `start_time` may lie anywhere relative to the current ledger time. The
// design decision, documented on `create_stream` and in the build spec §2.4:
// there is **no bound on clock skew** in either direction, and a future
// stream may extend beyond the `max_entry_ttl` horizon (creation funds as
// much rent as the network allows; the permissionless keeper path covers the
// rest, exactly as for multi-year streams). These tests pin that decision so
// a future tightening cannot land silently.

#[test]
fn backdated_start_vests_immediately() {
    let h = Harness::new();
    let start = h.now() - 50 * DAY;
    let id = h.create(100 * ONE, start, start + 100 * DAY, start, true, true, true);

    // Backdated vesting from a hire date is legitimate; half the schedule has
    // already elapsed, so half is already withdrawable.
    assert_eq!(h.client.vested_of(&id), 50 * ONE);
}

/// A stream created at the current ledger time has nothing vested yet — the
/// accrual clock has not advanced — and stays that way until time passes.
#[test]
fn a_stream_starting_now_vests_nothing_until_time_passes() {
    let h = Harness::new();
    let start = h.now();
    let id = h.create(100 * ONE, start, start + 100 * DAY, start, true, true, true);

    assert_eq!(h.client.vested_of(&id), 0);
    assert_eq!(h.client.refundable_of(&id), 100 * ONE);
    assert_eq!(
        h.client.try_withdraw(&id, &None).unwrap_err().unwrap(),
        Error::NothingToWithdraw,
    );

    // One second later the first stroops have accrued (rate ≈ 115 stroops/s).
    h.advance(1);
    assert!(h.client.vested_of(&id) >= 1);
}

/// A future start is a scheduled stream: nothing vests until the start
/// instant, then accrual runs to the end exactly as for a stream created
/// today. The sender's deposit sits in the pool meanwhile.
#[test]
fn a_future_start_vests_nothing_until_the_stream_opens() {
    let h = Harness::new();
    let start = h.now() + 30 * DAY;
    let id = h.create(100 * ONE, start, start + 100 * DAY, start, true, true, true);

    assert_eq!(h.client.vested_of(&id), 0, "nothing before the start");
    assert_eq!(h.client.withdrawable_of(&id), 0);
    assert_eq!(h.client.refundable_of(&id), 100 * ONE);
    h.assert_pool_exact();

    // Half a day before the start: still nothing.
    h.warp_to(start - 12 * 3600);
    assert_eq!(h.client.vested_of(&id), 0);

    // At the start instant accrual begins; a quarter of the way through it
    // has vested a quarter; at the end it is fully vested.
    h.warp_to(start);
    assert_eq!(h.client.vested_of(&id), 0);
    h.warp_to(start + 25 * DAY);
    assert_eq!(h.client.vested_of(&id), 25 * ONE);
    h.warp_to(start + 100 * DAY);
    assert_eq!(h.client.vested_of(&id), 100 * ONE);
}

/// A schedule whose *end* is already in the past is degenerate but safe: it
/// reads as fully vested and fully withdrawable the moment it is created. The
/// sender authorized the immediate vesting by supplying the schedule, and the
/// contract's job is to keep the numbers honest, which it does. The entry
/// still receives the 30-day TTL floor so the unclaimed tail stays readable.
#[test]
fn a_fully_elapsed_schedule_vests_immediately_in_full() {
    let h = Harness::new();

    // Both endpoints in the past.
    let past = h.now() - 200 * DAY;
    let id = h.create(100 * ONE, past, past + 100 * DAY, past, true, true, true);
    assert_eq!(h.client.vested_of(&id), 100 * ONE);
    assert_eq!(h.client.withdrawable_of(&id), 100 * ONE);
    assert_eq!(h.client.refundable_of(&id), 0);
    assert_eq!(h.ttl_of(id), storage::MIN_STREAM_TTL_LEDGERS);

    // Boundary: end exactly at the current ledger time is the same story.
    let boundary = h.create(
        100 * ONE,
        h.now() - 100 * DAY,
        h.now(),
        h.now() - 100 * DAY,
        true,
        true,
        true,
    );
    assert_eq!(h.client.vested_of(&boundary), 100 * ONE);
    h.assert_pool_exact();
}

/// **Design decision, pinned:** a future stream may be created even when its
/// schedule lies entirely beyond the TTL horizon — the same decision the
/// contract already makes for multi-year streams. Creation funds the entry
/// for as long as the network allows; the permissionless keeper path covers
/// the rest. Rejecting far-future streams would forbid a 150-day-off start
/// while accepting a five-year stream, which would be incoherent.
#[test]
fn a_future_stream_may_extend_beyond_the_ttl_horizon() {
    let h = Harness::new();
    const MAX_TTL: u32 = 100_000;
    h.env.ledger().set_max_entry_ttl(MAX_TTL);

    // Starts 150 days out and runs 100 days: even the wait alone exceeds the
    // rent window.
    let start = h.now() + 150 * DAY;
    let id = h.create(
        1_000 * ONE,
        start,
        start + 100 * DAY,
        start,
        true,
        true,
        true,
    );

    assert_eq!(h.ttl_of(id), MAX_TTL, "creation clamps, never exceeds");
    assert!(
        storage::seconds_to_ledgers(250 * DAY) > MAX_TTL,
        "this test is only meaningful if the wait exceeds the max TTL",
    );
    assert_eq!(h.client.vested_of(&id), 0);
    assert_eq!(h.client.withdrawable_of(&id), 0);
    assert_eq!(h.client.refundable_of(&id), 1_000 * ONE);

    // Once the stream opens it accrues normally.
    h.warp_to(start + 50 * DAY);
    assert_eq!(h.client.vested_of(&id), 500 * ONE);
    h.assert_pool_exact();
}

/// `end_time == start_time + 1` is the shortest schedule there is: one second
/// of streaming, with a one-stroop deposit clearing the rate floor exactly.
#[test]
fn a_one_second_stream_is_the_shortest_allowed_schedule() {
    let h = Harness::new();
    let start = h.now();
    let id = h.create(1, start, start + 1, start, true, true, true);

    h.advance(1);
    assert_eq!(h.client.vested_of(&id), 1);
    assert_eq!(h.client.withdraw(&id, &None), 1);
    h.assert_pool_exact();
}

/// A rejected create consumes no id and moves no tokens, so a retry after a
/// client-side validation miss is deterministic: the corrected attempt simply
/// gets the first id.
#[test]
fn a_rejected_create_leaves_no_residue_for_a_retry() {
    let h = Harness::new();
    let start = h.now();
    let pool_before = h.pool();

    let expect = |err: Error, deposit: i128, s: u64, e: u64, c: u64| {
        let got = h
            .client
            .try_create_stream(
                &h.sender,
                &h.recipient,
                &h.token,
                &deposit,
                &s,
                &e,
                &c,
                &true,
                &true,
                &true,
            )
            .unwrap_err()
            .unwrap();
        assert_eq!(got, err);
    };

    expect(Error::InvalidTimeRange, 100 * ONE, start, start, start);
    expect(Error::InvalidTimeRange, 100 * ONE, start, start - 1, start);
    expect(
        Error::InvalidCliff,
        100 * ONE,
        start,
        start + DAY,
        start - 1,
    );
    expect(
        Error::InvalidCliff,
        100 * ONE,
        start,
        start + DAY,
        start + DAY + 1,
    );
    expect(Error::InvalidDeposit, 0, start, start + DAY, start);

    assert_eq!(h.client.stream_count(), 0, "failed creates consumed an id");
    assert_eq!(h.pool(), pool_before, "failed creates moved tokens");

    // The retry with corrected inputs succeeds and gets the first id.
    let id = h.create(100 * ONE, start, start + DAY, start, true, true, true);
    assert_eq!(id, 0);
    assert_eq!(h.pool(), pool_before + 100 * ONE);
    h.assert_pool_exact();
}

#[test]
fn unknown_stream_id_is_a_typed_error() {
    let h = Harness::new();
    let err = h.client.try_get_stream(&999).unwrap_err().unwrap();
    assert_eq!(err, Error::StreamNotFound);
    assert!(!h.client.stream_exists(&999));
}

/// Independent tokens must not be able to satisfy one another's liabilities.
#[test]
fn streams_of_different_tokens_are_accounted_separately() {
    let h = Harness::new();
    let issuer = Address::generate(&h.env);
    let other_asset = h.env.register_stellar_asset_contract_v2(issuer);
    let other_token = other_asset.address();
    soroban_sdk::token::StellarAssetClient::new(&h.env, &other_token)
        .mint(&h.sender, &(1_000 * ONE));

    let start = h.now();
    h.create_simple(100 * ONE, 100 * DAY);
    h.client.create_stream(
        &h.sender,
        &h.recipient,
        &other_token,
        &(200 * ONE),
        &start,
        &(start + 100 * DAY),
        &start,
        &true,
        &true,
        &true,
    );

    assert_eq!(h.pool(), 100 * ONE, "first token pool");
    assert_eq!(
        soroban_sdk::token::Client::new(&h.env, &other_token).balance(&h.contract_id),
        200 * ONE,
        "second token pool",
    );
    h.assert_pool_exact();
}
