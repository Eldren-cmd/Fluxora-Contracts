#![no_std]
//! # Fluxora — continuous payment streaming for Soroban
//!
//! Lock tokens once; have them accrue continuously to a recipient over time.
//! The recipient pulls their accrued balance whenever they like.
//!
//! This contract is a *primitive*, not an application. Payroll tools, grant
//! programs, subscription billing and vesting schedules are meant to be built
//! on top of it. Every scoping decision below favours generality on chain and
//! pushes convenience to the SDK.
//!
//! ## Pull-based by necessity
//!
//! Stellar has no scheduler — no cron, no keeper network, no way for a contract
//! to wake itself up. Every state change must be triggered by an external
//! transaction. So nothing here runs in the background: the recipient calls
//! [`FluxoraStream::withdraw`] and the contract computes what they have earned
//! at that instant.
//!
//! ## No on-chain stream discovery
//!
//! There is deliberately no per-user list of stream ids in storage. A `Vec<u64>`
//! of a treasury's streams grows without bound, costs rent forever, and blows
//! Soroban's ~200-ledger-entry read limit once that treasury has a few hundred
//! recipients. On chain, a stream is only ever addressed by its `u64` id.
//!
//! Discovery is an off-chain concern: [`create_stream`](FluxoraStream::create_stream)
//! returns the new id and emits an event carrying sender, recipient and every
//! schedule field, so an indexer can answer "show me my streams" without the
//! contract paying rent to remember.
//!
//! ## Immutable guarantees
//!
//! `cancellable`, `pausable` and `transferable` are fixed at creation and can
//! never change afterwards. This is a trust feature: before accepting a stream a
//! recipient can verify that the sender cannot claw it back, freeze it, or
//! reassign it. A stream that could *become* cancellable later would be
//! worthless as a guarantee.
//!
//! For the same reason the contract has no admin key, no upgrade path, no fee
//! switch and no global pause. Immutability is what lets another protocol depend
//! on this one.

// The test suite runs against the host with `std` available; the contract
// itself is strictly `no_std`.
#[cfg(test)]
extern crate std;

mod accrual;
mod error;
mod events;
mod storage;
mod types;

pub use accrual::{
    cliff_reached, duration, elapsed, liability, refundable, stream_time, vested, withdrawable,
};
pub use error::Error;
pub use storage::{SECONDS_PER_LEDGER, TTL_BUFFER_SECONDS};
pub use types::{DataKey, Stream, StreamStatus};

use soroban_sdk::{contract, contractimpl, token, Address, Env, MuxedAddress};

#[contract]
pub struct FluxoraStream;

#[contractimpl]
impl FluxoraStream {
    // ---------------------------------------------------------------------
    // Lifecycle
    // ---------------------------------------------------------------------

    /// Create a stream and move `deposit` from `sender` into the contract's
    /// pooled balance.
    ///
    /// Returns the new stream id. The id is monotonic and never reused, so it is
    /// a stable handle for an indexer.
    ///
    /// # Schedule
    ///
    /// Tokens accrue linearly from `start_time` to `end_time`. `start_time` may
    /// be in the past — backdated vesting from a hire date or grant award date
    /// is a legitimate use — in which case the backdated portion is immediately
    /// withdrawable.
    ///
    /// `cliff_time` **gates** the payout, it does not delay accrual. Pass
    /// `cliff_time == start_time` for no cliff. At the cliff instant the
    /// recipient becomes entitled to everything accrued since `start_time`, not
    /// merely what accrues after the cliff. This is standard vesting semantics
    /// and it surprises people, so it is worth restating in any UI.
    ///
    /// # Errors
    ///
    /// * [`Error::SelfStream`] — sender and recipient are the same address.
    /// * [`Error::InvalidDeposit`] — deposit is not positive.
    /// * [`Error::InvalidTimeRange`] — `end_time <= start_time`.
    /// * [`Error::InvalidCliff`] — cliff outside `[start_time, end_time]`.
    /// * [`Error::DepositRateTooLow`] — `deposit < duration`, so the per-second
    ///   rate would truncate to zero and the recipient would accrue nothing.
    /// * [`Error::Overflow`] — `deposit * duration` does not fit in `i128`.
    #[allow(clippy::too_many_arguments)]
    pub fn create_stream(
        env: Env,
        sender: Address,
        recipient: Address,
        token: Address,
        deposit: i128,
        start_time: u64,
        end_time: u64,
        cliff_time: u64,
        cancellable: bool,
        pausable: bool,
        transferable: bool,
    ) -> Result<u64, Error> {
        sender.require_auth();

        if sender == recipient {
            return Err(Error::SelfStream);
        }
        if deposit <= 0 {
            return Err(Error::InvalidDeposit);
        }
        if end_time <= start_time {
            return Err(Error::InvalidTimeRange);
        }
        if cliff_time < start_time || cliff_time > end_time {
            return Err(Error::InvalidCliff);
        }

        let total_duration = end_time - start_time;

        // Reject dust-rate streams. Below one stroop per second the recipient
        // accrues literally nothing until very late in the schedule, which is a
        // real footgun for a treasury streaming a small grant over a year.
        if deposit < total_duration as i128 {
            return Err(Error::DepositRateTooLow);
        }

        // Front-load the overflow guard for all future accrual. Because
        // `elapsed <= duration` always holds, proving `deposit * duration` fits
        // in an i128 here means the `deposited * elapsed` multiplication inside
        // `vested` can never overflow for the life of the stream. `top_up`
        // re-establishes the same guard against its new figures.
        deposit
            .checked_mul(total_duration as i128)
            .ok_or(Error::Overflow)?;

        let stream_id = storage::next_stream_id(&env)?;
        let stream = Stream {
            sender: sender.clone(),
            recipient,
            token: token.clone(),
            deposited: deposit,
            withdrawn: 0,
            start_time,
            end_time,
            cliff_time,
            cancellable,
            pausable,
            transferable,
            paused_at: None,
            paused_total: 0,
            status: StreamStatus::Active,
        };

        storage::save_stream(&env, stream_id, &stream);
        storage::extend_instance(&env);

        // Pull the deposit in last. The sender's auth on this invocation covers
        // the nested token transfer, so no prior approval is needed.
        token::Client::new(&env, &token).transfer(
            &sender,
            MuxedAddress::from(env.current_contract_address()),
            &deposit,
        );

        events::stream_created(&env, stream_id, &stream);
        Ok(stream_id)
    }

    /// Withdraw accrued tokens to the recipient.
    ///
    /// `amount == None` withdraws the full withdrawable balance. Returns the
    /// amount actually transferred.
    ///
    /// Withdrawal works while the stream is paused: pausing stops *accrual*, it
    /// does not freeze funds the recipient has already earned. Freezing earned
    /// funds would make pausable streams unacceptable to any serious recipient.
    ///
    /// # Errors
    ///
    /// * [`Error::NothingToWithdraw`] — withdrawable balance is zero. A typed
    ///   error rather than a silent no-op, so a caller can tell the difference
    ///   between "nothing yet" and "transferred zero".
    /// * [`Error::InsufficientWithdrawable`] — explicit amount exceeds the
    ///   withdrawable balance.
    pub fn withdraw(env: Env, stream_id: u64, amount: Option<i128>) -> Result<i128, Error> {
        let mut stream = storage::load_stream(&env, stream_id)?;
        stream.recipient.require_auth();

        let now = env.ledger().timestamp();
        let available = accrual::withdrawable(&stream, now)?;
        if available == 0 {
            return Err(Error::NothingToWithdraw);
        }

        let payout = match amount {
            None => available,
            Some(requested) => {
                if requested <= 0 {
                    return Err(Error::InvalidAmount);
                }
                if requested > available {
                    return Err(Error::InsufficientWithdrawable);
                }
                requested
            }
        };

        Self::apply_withdrawal(&env, stream_id, &mut stream, payout)?;
        Ok(payout)
    }

    // ---------------------------------------------------------------------
    // Views
    // ---------------------------------------------------------------------

    /// Full stream state.
    ///
    /// Views deliberately do **not** extend the entry's TTL. They are called
    /// through simulation by the SDK and UI, where a write to the footprint is
    /// at best noise and at worst confusing. Keeping a stream alive is the
    /// explicit job of [`extend_stream_ttl`](Self::extend_stream_ttl).
    pub fn get_stream(env: Env, stream_id: u64) -> Result<Stream, Error> {
        storage::peek_stream(&env, stream_id)
    }

    /// Amount the recipient could withdraw right now.
    pub fn withdrawable_of(env: Env, stream_id: u64) -> Result<i128, Error> {
        let stream = storage::peek_stream(&env, stream_id)?;
        accrual::withdrawable(&stream, env.ledger().timestamp())
    }

    /// Total earned by the recipient since `start_time`, withdrawn or not.
    pub fn vested_of(env: Env, stream_id: u64) -> Result<i128, Error> {
        let stream = storage::peek_stream(&env, stream_id)?;
        accrual::vested(&stream, env.ledger().timestamp())
    }

    /// Amount that would be refunded to the sender if they cancelled right now.
    pub fn refundable_of(env: Env, stream_id: u64) -> Result<i128, Error> {
        let stream = storage::peek_stream(&env, stream_id)?;
        accrual::refundable(&stream, env.ledger().timestamp())
    }

    /// Number of streams ever created. Ids run `0..stream_count()`.
    pub fn stream_count(env: Env) -> u64 {
        storage::stream_count(&env)
    }

    /// Whether a stream entry is currently readable.
    ///
    /// Returns `false` both for ids that were never issued and for entries that
    /// have been archived. Compare against [`stream_count`](Self::stream_count)
    /// to tell those apart: an id below the count that does not exist has been
    /// archived and needs restoring.
    pub fn stream_exists(env: Env, stream_id: u64) -> bool {
        storage::stream_exists(&env, stream_id)
    }

    // ---------------------------------------------------------------------
    // Maintenance
    // ---------------------------------------------------------------------

    // ---------------------------------------------------------------------
    // Internal
    // ---------------------------------------------------------------------

    /// Shared tail of [`withdraw`](Self::withdraw) and
    /// [`batch_withdraw`](Self::batch_withdraw): update accounting, persist,
    /// pay out, emit.
    ///
    /// State is written before the token call (checks-effects-interactions).
    /// Soroban forbids reentrancy outright, so this is belt-and-braces rather
    /// than load-bearing, but it keeps the ordering obvious to a reader.
    fn apply_withdrawal(
        env: &Env,
        stream_id: u64,
        stream: &mut Stream,
        payout: i128,
    ) -> Result<(), Error> {
        stream.withdrawn = stream
            .withdrawn
            .checked_add(payout)
            .ok_or(Error::Overflow)?;

        // `Cancelled` is sticky: draining a cancelled stream to zero leaves it
        // visibly cancelled rather than relabelling it as a clean completion.
        if stream.withdrawn >= stream.deposited && stream.status != StreamStatus::Cancelled {
            stream.status = StreamStatus::Depleted;
        }

        let token = stream.token.clone();
        let recipient = stream.recipient.clone();
        storage::save_stream(env, stream_id, stream);

        token::Client::new(env, &token).transfer(
            &env.current_contract_address(),
            MuxedAddress::from(recipient),
            &payout,
        );

        events::withdrawn(env, stream_id, stream, payout);
        Ok(())
    }
}

#[cfg(test)]
#[path = "test/mod.rs"]
mod test;
