//! Stage 2 — top-up.
//!
//! Chosen semantics: **extend the duration, keep the rate**. The per-second
//! rate the recipient agreed to at creation never changes; `end_time` moves
//! forward instead. These tests pin that down, because the alternative
//! (hold `end_time`, raise the rate) is retroactive and would silently re-vest
//! elapsed time.

use super::common::*;
use crate::{Error, StreamStatus};

#[test]
fn top_up_extends_the_end_date_at_the_same_rate() {
    let h = Harness::new();
    // 1000 tokens over 100 days = 10/day.
    let id = h.create_simple(1_000 * ONE, 100 * DAY);
    let original_end = h.get(id).end_time;

    h.client.top_up(&id, &(100 * ONE));
    let s = h.get(id);

    assert_eq!(s.deposited, 1_100 * ONE);
    assert_eq!(
        s.end_time,
        original_end + 10 * DAY,
        "100 tokens at 10/day = 10 days"
    );
    assert_eq!(h.pool(), 1_100 * ONE);
    h.assert_pool_exact();
}

/// The defining property: a top-up must not change what is already withdrawable.
#[test]
fn top_up_does_not_retroactively_vest_elapsed_time() {
    let h = Harness::new();
    let id = h.create_simple(1_000 * ONE, 100 * DAY);

    h.advance(50 * DAY);
    let before = h.client.vested_of(&id);
    assert_eq!(before, 500 * ONE);

    h.client.top_up(&id, &(1_000 * ONE));

    assert_eq!(
        h.client.vested_of(&id),
        before,
        "topping up must not move already-vested funds",
    );
}

#[test]
fn the_per_second_rate_survives_a_top_up() {
    let h = Harness::new();
    let id = h.create_simple(1_000 * ONE, 100 * DAY);

    h.advance(50 * DAY);
    h.client.top_up(&id, &(500 * ONE));

    // Still 10 tokens/day.
    let before = h.client.vested_of(&id);
    h.advance(10 * DAY);
    assert_eq!(h.client.vested_of(&id) - before, 100 * ONE);
}

#[test]
fn a_topped_up_stream_eventually_delivers_everything() {
    let h = Harness::new();
    let id = h.create_simple(1_000 * ONE, 100 * DAY);
    h.advance(50 * DAY);
    h.client.top_up(&id, &(500 * ONE));

    let end = h.get(id).end_time;
    h.warp_to(end);

    assert_eq!(h.client.vested_of(&id), 1_500 * ONE);
    assert_eq!(h.client.withdraw(&id, &None), 1_500 * ONE);
    assert_eq!(h.pool(), 0);
    h.assert_pool_exact();
}

#[test]
fn repeated_top_ups_compound_correctly() {
    let h = Harness::new();
    let id = h.create_simple(1_000 * ONE, 100 * DAY);

    for _ in 0..5 {
        h.advance(5 * DAY);
        h.client.top_up(&id, &(100 * ONE));
    }

    let s = h.get(id);
    assert_eq!(s.deposited, 1_500 * ONE);
    assert_eq!(s.end_time, T0 + 150 * DAY, "5 x 10 days of extension");

    h.warp_to(s.end_time);
    assert_eq!(h.client.withdraw(&id, &None), 1_500 * ONE);
    h.assert_pool_exact();
}

#[test]
fn top_up_works_after_a_partial_withdrawal() {
    let h = Harness::new();
    let id = h.create_simple(1_000 * ONE, 100 * DAY);
    h.advance(30 * DAY);
    h.client.withdraw(&id, &None);

    h.client.top_up(&id, &(200 * ONE));
    assert_eq!(h.get(id).deposited, 1_200 * ONE);
    assert_eq!(h.pool(), 900 * ONE, "700 unvested + 200 new");
    h.assert_pool_exact();

    h.warp_to(h.get(id).end_time);
    assert_eq!(h.client.withdraw(&id, &None), 900 * ONE);
    assert_eq!(h.balance(&h.recipient), 1_200 * ONE);
    h.assert_pool_exact();
}

#[test]
fn top_up_is_allowed_while_paused_and_does_not_resume() {
    let h = Harness::new();
    let id = h.create_simple(1_000 * ONE, 100 * DAY);
    h.advance(30 * DAY);
    h.client.pause(&id);

    h.client.top_up(&id, &(100 * ONE));

    let s = h.get(id);
    assert_eq!(s.status, StreamStatus::Paused);
    assert_eq!(s.deposited, 1_100 * ONE);
    assert_eq!(h.client.vested_of(&id), 300 * ONE, "still frozen");
    h.assert_pool_exact();
}

/// The duration extension rounds up, so the effective rate after a top-up is
/// never faster than the original.
#[test]
fn extension_rounds_up_so_the_rate_never_accelerates() {
    let h = Harness::new();
    let start = h.now();
    // 1000 stroops over 300 seconds: 3.33 stroops/sec, deliberately inexact.
    let id = h.create(1_000, start, start + 300, start, true, true, true);

    h.client.top_up(&id, &1);
    let s = h.get(id);

    // 1 stroop at 1000/300 per second is 0.3s, rounded up to 1s.
    assert_eq!(s.end_time, start + 301);
    assert_eq!(s.deposited, 1_001);

    // The new rate must not exceed the old one at any sampled point.
    for t in [1u64, 50, 150, 300] {
        h.warp_to(start + t);
        let new_rate_vested = h.client.vested_of(&id);
        let old_rate_vested = 1_000 * t as i128 / 300;
        assert!(
            new_rate_vested <= old_rate_vested,
            "at t+{t}: topped-up stream vested {new_rate_vested} > original {old_rate_vested}",
        );
    }
}

// --- Guards ---------------------------------------------------------------

/// Topping up a matured stream would make the new funds instantly withdrawable,
/// which is never what the sender means.
#[test]
fn topping_up_a_matured_stream_is_rejected() {
    let h = Harness::new();
    let id = h.create_simple(1_000 * ONE, 100 * DAY);
    h.warp_to(T0 + 100 * DAY);

    let err = h.client.try_top_up(&id, &(100 * ONE)).unwrap_err().unwrap();
    assert_eq!(err, Error::StreamMatured);

    h.advance(YEAR);
    let err = h.client.try_top_up(&id, &(100 * ONE)).unwrap_err().unwrap();
    assert_eq!(err, Error::StreamMatured);

    assert_eq!(
        h.pool(),
        1_000 * ONE,
        "no funds pulled by a rejected top-up"
    );
    h.assert_pool_exact();
}

/// One second before maturity is still fine — the boundary is exact.
#[test]
fn topping_up_one_second_before_maturity_is_allowed() {
    let h = Harness::new();
    let id = h.create_simple(1_000 * ONE, 100 * DAY);
    h.warp_to(T0 + 100 * DAY - 1);

    h.client.top_up(&id, &(100 * ONE));
    assert_eq!(h.get(id).deposited, 1_100 * ONE);
    h.assert_pool_exact();
}

#[test]
fn topping_up_a_cancelled_stream_is_rejected() {
    let h = Harness::new();
    let id = h.create_simple(1_000 * ONE, 100 * DAY);
    h.advance(30 * DAY);
    h.client.cancel(&id);

    let err = h.client.try_top_up(&id, &(100 * ONE)).unwrap_err().unwrap();
    assert_eq!(err, Error::StreamTerminated);
    h.assert_pool_exact();
}

#[test]
fn topping_up_a_depleted_stream_is_rejected() {
    let h = Harness::new();
    let id = h.create_simple(1_000 * ONE, 10 * DAY);
    h.advance(10 * DAY);
    h.client.withdraw(&id, &None);

    let err = h.client.try_top_up(&id, &(100 * ONE)).unwrap_err().unwrap();
    assert_eq!(err, Error::StreamTerminated);
}

#[test]
fn non_positive_top_up_is_rejected() {
    let h = Harness::new();
    let id = h.create_simple(1_000 * ONE, 100 * DAY);

    for amount in [0i128, -1, -100 * ONE] {
        let err = h.client.try_top_up(&id, &amount).unwrap_err().unwrap();
        assert_eq!(err, Error::InvalidAmount, "amount {amount}");
    }
    assert_eq!(h.pool(), 1_000 * ONE);
}

#[test]
fn a_top_up_that_would_overflow_accrual_is_rejected() {
    let h = Harness::new();
    let id = h.create_simple(1_000 * ONE, 100 * DAY);

    let err = h
        .client
        .try_top_up(&id, &(i128::MAX / 2))
        .unwrap_err()
        .unwrap();
    assert_eq!(err, Error::Overflow);
    h.assert_pool_exact();
}
