//! NSE market data plumbing, ported from kstocks_egui: shared HTTP client,
//! NSE-clock-derived session state, F&O symbol/expiry resolution and the
//! live WSS streamers. Streamers are decoupled from storage and emit ticks
//! through channels/callbacks so the chain engine and UI can consume them.

pub mod events;
pub mod http;
pub mod market_clock;
pub mod status;
pub mod streamers;
pub mod symbols;
