//! Event definitions and emission.
//!
//! Stream discovery is an off-chain concern — the contract keeps no per-user
//! index (see the `lib.rs` module docs). That makes these events the *only* way
//! an indexer learns that a stream exists or that its state moved, so they are
//! load-bearing infrastructure rather than optional telemetry.
//!
//! # Contract
//!
//! * Every state change emits exactly one event.
//! * Events are declared with `#[contractevent]`, so their schemas land in the
//!   contract's interface spec. Tooling and the TypeScript SDK generate typed
//!   decoders from that spec instead of hand-rolling topic parsers.
//! * The static topic is the struct name in snake_case. `stream_id` is always a
//!   topic, as are the addresses an indexer routes on, so a consumer can filter
//!   server-side by event kind, by stream, or by party.
//! * Each payload carries enough state to reconstruct the stream without
//!   replaying from genesis.
//!
//! Field order and topic placement are ABI. Adding a field is a compatible
//! change; reordering or re-topicking one is not.

use soroban_sdk::{contractevent, Address, Env};

use crate::types::{Stream, StreamStatus};

/// A new stream was created. Carries the complete initial state — this is the
/// event an indexer builds its sender/recipient mapping from.
#[contractevent]
pub struct StreamCreated {
    #[topic]
    pub stream_id: u64,
    #[topic]
    pub sender: Address,
    #[topic]
    pub recipient: Address,
    pub token: Address,
    pub deposited: i128,
    pub start_time: u64,
    pub end_time: u64,
    pub cliff_time: u64,
    pub cancellable: bool,
    pub pausable: bool,
    pub transferable: bool,
}

/// The recipient drew down accrued funds. Emitted once per stream, including
/// once per drawn-from stream inside a `batch_withdraw`.
#[contractevent]
pub struct Withdrawn {
    #[topic]
    pub stream_id: u64,
    #[topic]
    pub recipient: Address,
    /// Amount moved in this call.
    pub amount: i128,
    /// Cumulative withdrawn after this call.
    pub withdrawn: i128,
    pub deposited: i128,
    pub status: StreamStatus,
}

// ---------------------------------------------------------------------------
// Emission helpers
// ---------------------------------------------------------------------------

pub fn stream_created(env: &Env, stream_id: u64, stream: &Stream) {
    StreamCreated {
        stream_id,
        sender: stream.sender.clone(),
        recipient: stream.recipient.clone(),
        token: stream.token.clone(),
        deposited: stream.deposited,
        start_time: stream.start_time,
        end_time: stream.end_time,
        cliff_time: stream.cliff_time,
        cancellable: stream.cancellable,
        pausable: stream.pausable,
        transferable: stream.transferable,
    }
    .publish(env);
}

pub fn withdrawn(env: &Env, stream_id: u64, stream: &Stream, amount: i128) {
    Withdrawn {
        stream_id,
        recipient: stream.recipient.clone(),
        amount,
        withdrawn: stream.withdrawn,
        deposited: stream.deposited,
        status: stream.status,
    }
    .publish(env);
}
