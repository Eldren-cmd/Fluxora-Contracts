//! Test suite, staged to match the build order.

mod common;

// Stage 1
mod create;
mod props;
mod withdraw;

// Stage 2
mod auth;
mod cancel;
mod cliff;
mod pause;
mod top_up;
mod transfer;
