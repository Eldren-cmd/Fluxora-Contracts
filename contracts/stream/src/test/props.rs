//! Stage 1 — property tests over the accrual math.
//!
//! These drive [`crate::accrual`] directly rather than going through the
//! contract. The functions there are pure, so a case is a few microseconds
//! instead of a host invocation, which buys enough cases to actually explore
//! the space of schedules.
//!
//! # The conservation property
//!
//! The headline invariant is stronger than "dust is bounded":
//!
//! ```text
//! vested(t) + refundable(t) == deposited      for all t
//! ```
//!
//! *Exactly*, with no dust term at all. That falls out of computing `vested`
//! from the cumulative formula `deposited * elapsed / duration` rather than by
//! summing per-interval deltas. Truncation error therefore never accumulates:
//! it is re-derived from scratch on every call and bounded by one stroop at any
//! instant, and it vanishes entirely once the stream settles, because
//! `refundable` is *defined* as the complement of `vested`.
//!
//! A per-interval implementation — the obvious one, and the one the existing
//! MVPs use — loses a stroop per withdrawal and strands it in the pool forever.

use proptest::prelude::*;
use soroban_sdk::testutils::Address as _;
use soroban_sdk::{Address, Env};

use crate::accrual;
use crate::types::{Stream, StreamStatus};

/// Build a stream directly, bypassing the contract, so a property case costs
/// no host invocations.
fn stream_of(deposited: i128, start: u64, duration: u64, cliff_offset: u64) -> Stream {
    let env = Env::default();
    Stream {
        sender: Address::generate(&env),
        recipient: Address::generate(&env),
        token: Address::generate(&env),
        deposited,
        withdrawn: 0,
        start_time: start,
        end_time: start + duration,
        cliff_time: start + cliff_offset,
        cancellable: true,
        pausable: true,
        transferable: true,
        paused_at: None,
        paused_total: 0,
        status: StreamStatus::Active,
    }
}

/// Mirrors `create_stream`'s validation so generated cases are ones the
/// contract would actually accept.
fn valid(deposited: i128, duration: u64, cliff_offset: u64) -> bool {
    duration > 0
        && deposited > 0
        && deposited >= duration as i128
        && cliff_offset <= duration
        && deposited.checked_mul(duration as i128).is_some()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Vesting is bounded below by zero and above by the deposit, at every
    /// instant, including far past the end and far before the start.
    #[test]
    fn vested_stays_within_zero_and_deposited(
        deposited in 1i128..i128::MAX / (1 << 40),
        duration in 1u64..(20 * 365 * 86_400),
        cliff_frac in 0u64..=100,
        offset in -1_000_000i64..(40 * 365 * 86_400),
    ) {
        let cliff_offset = duration * cliff_frac / 100;
        prop_assume!(valid(deposited, duration, cliff_offset));

        let start = 1_700_000_000u64;
        let s = stream_of(deposited, start, duration, cliff_offset);
        let now = (start as i64 + offset).max(0) as u64;

        let v = accrual::vested(&s, now).unwrap();
        prop_assert!(v >= 0, "vested went negative: {}", v);
        prop_assert!(v <= deposited, "vested {} exceeded deposit {}", v, deposited);
    }

    /// **Conservation.** What the recipient has earned plus what the sender
    /// would get back on cancellation is always exactly the deposit. No dust,
    /// no leak, at any instant.
    #[test]
    fn vested_plus_refundable_equals_deposited(
        deposited in 1i128..i128::MAX / (1 << 40),
        duration in 1u64..(20 * 365 * 86_400),
        cliff_frac in 0u64..=100,
        elapsed in 0u64..(40 * 365 * 86_400),
    ) {
        let cliff_offset = duration * cliff_frac / 100;
        prop_assume!(valid(deposited, duration, cliff_offset));

        let start = 1_700_000_000u64;
        let s = stream_of(deposited, start, duration, cliff_offset);
        let now = start + elapsed;

        let v = accrual::vested(&s, now).unwrap();
        let r = accrual::refundable(&s, now).unwrap();
        prop_assert_eq!(v + r, deposited);
    }

    /// Vesting never goes backwards. If it could, `withdrawn` would be able to
    /// exceed `vested` and the withdrawable calculation would underflow.
    #[test]
    fn vested_is_monotonic_in_time(
        deposited in 1i128..i128::MAX / (1 << 40),
        duration in 1u64..(20 * 365 * 86_400),
        cliff_frac in 0u64..=100,
        t in 0u64..(20 * 365 * 86_400),
        step in 1u64..(365 * 86_400),
    ) {
        let cliff_offset = duration * cliff_frac / 100;
        prop_assume!(valid(deposited, duration, cliff_offset));

        let start = 1_700_000_000u64;
        let s = stream_of(deposited, start, duration, cliff_offset);

        let earlier = accrual::vested(&s, start + t).unwrap();
        let later = accrual::vested(&s, start + t + step).unwrap();
        prop_assert!(later >= earlier, "vesting went backwards: {} -> {}", earlier, later);
    }

    /// Rounding is **down**, and tight to within one stroop.
    ///
    /// Truncating in the recipient's disfavour is the correct direction: the
    /// residue stays in the pool and returns to the sender at settlement, so
    /// the contract can never owe more than it holds.
    #[test]
    fn vesting_rounds_down_and_is_tight(
        deposited in 1i128..i128::MAX / (1 << 40),
        duration in 2u64..(20 * 365 * 86_400),
        elapsed in 1u64..(20 * 365 * 86_400),
    ) {
        prop_assume!(valid(deposited, duration, 0));
        prop_assume!(elapsed < duration);

        let start = 1_700_000_000u64;
        let s = stream_of(deposited, start, duration, 0);
        let v = accrual::vested(&s, start + elapsed).unwrap();

        let d = duration as i128;
        let e = elapsed as i128;
        // v == floor(deposited * elapsed / duration)
        prop_assert!(v * d <= deposited * e, "rounded up");
        prop_assert!((v + 1) * d > deposited * e, "rounded down too far");
    }

    /// Before the cliff the entitlement is exactly zero; at the cliff instant
    /// the recipient is owed everything accrued since `start_time`, not merely
    /// what accrues after the cliff.
    #[test]
    fn cliff_gates_but_does_not_delay(
        deposited in 1i128..i128::MAX / (1 << 40),
        duration in 100u64..(20 * 365 * 86_400),
        cliff_frac in 1u64..100,
    ) {
        let cliff_offset = (duration * cliff_frac / 100).max(1);
        prop_assume!(valid(deposited, duration, cliff_offset));

        let start = 1_700_000_000u64;
        let s = stream_of(deposited, start, duration, cliff_offset);

        prop_assert_eq!(accrual::vested(&s, start + cliff_offset - 1).unwrap(), 0);

        let at_cliff = accrual::vested(&s, start + cliff_offset).unwrap();
        let expected = deposited * cliff_offset as i128 / duration as i128;
        prop_assert_eq!(at_cliff, expected, "cliff must release all prior accrual");
    }

    /// A full withdrawal schedule: draw at arbitrary times, then settle. The
    /// total paid out plus the final refund is exactly the deposit.
    #[test]
    fn withdrawal_schedule_conserves_the_deposit(
        deposited in 1i128..i128::MAX / (1 << 40),
        duration in 10u64..(10 * 365 * 86_400),
        cliff_frac in 0u64..=50,
        draw_fracs in prop::collection::vec(0u64..=120, 1..12),
    ) {
        let cliff_offset = duration * cliff_frac / 100;
        prop_assume!(valid(deposited, duration, cliff_offset));

        let start = 1_700_000_000u64;
        let mut s = stream_of(deposited, start, duration, cliff_offset);

        let mut paid_out = 0i128;
        let mut times: std::vec::Vec<u64> =
            draw_fracs.iter().map(|f| start + duration * f / 100).collect();
        times.sort_unstable();

        for &now in &times {
            let available = accrual::withdrawable(&s, now).unwrap();
            prop_assert!(available >= 0);
            s.withdrawn += available;
            paid_out += available;

            // The recipient can never have been paid more than they earned.
            prop_assert!(s.withdrawn <= accrual::vested(&s, now).unwrap());
            // Nor more than the pool holds for them.
            prop_assert!(s.withdrawn <= s.deposited);
        }

        // Settle at the last draw: everything paid out, plus everything the
        // sender would get back, is exactly the deposit. No dust either way.
        let settle_at = *times.last().unwrap();
        let refund = accrual::refundable(&s, settle_at).unwrap();
        prop_assert_eq!(paid_out + refund, deposited);

        // And a schedule that actually reached maturity paid out in full.
        if settle_at >= start + duration {
            prop_assert_eq!(paid_out, deposited);
            prop_assert_eq!(refund, 0);
        }
    }

    /// Pausing conserves value: the total delivered by the stretched schedule
    /// equals the total the unpaused schedule would have delivered.
    #[test]
    fn pausing_stretches_without_changing_total_value(
        deposited in 1i128..i128::MAX / (1 << 40),
        duration in 100u64..(10 * 365 * 86_400),
        pause_at_frac in 1u64..99,
        pause_len in 1u64..(2 * 365 * 86_400),
    ) {
        prop_assume!(valid(deposited, duration, 0));

        let start = 1_700_000_000u64;
        let mut s = stream_of(deposited, start, duration, 0);

        let pause_at = start + duration * pause_at_frac / 100;
        let at_pause = accrual::vested(&s, pause_at).unwrap();

        // Freeze.
        s.paused_at = Some(pause_at);
        s.status = StreamStatus::Paused;
        for probe in [0u64, 1, pause_len / 2, pause_len] {
            prop_assert_eq!(
                accrual::vested(&s, pause_at + probe).unwrap(),
                at_pause,
                "accrual continued while paused",
            );
        }

        // Resume, and confirm the clock picks up exactly where it stopped.
        s.paused_at = None;
        s.paused_total += pause_len;
        s.status = StreamStatus::Active;
        prop_assert_eq!(accrual::vested(&s, pause_at + pause_len).unwrap(), at_pause);

        // The stretched schedule still delivers the whole deposit, just later.
        let stretched_end = start + duration + pause_len;
        prop_assert_eq!(accrual::vested(&s, stretched_end).unwrap(), deposited);
        prop_assert!(accrual::vested(&s, start + duration).unwrap() < deposited);
    }
}
