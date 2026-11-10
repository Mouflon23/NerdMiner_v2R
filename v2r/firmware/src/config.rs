//! Tuning constants. The settings people change are in [`crate::settings`].

pub const AGENT: &str = concat!("NerdMinerV2R/", env!("CARGO_PKG_VERSION"));

/// About one share every 20 s at 350 kH/s: a difficulty-1 share takes 2³²
/// hashes.
pub const SUGGESTED_DIFFICULTY: f64 = 0.0015;

/// Hits below this are not worth reporting, whatever the pool asks for. Set well
/// under any pool difficulty so the best-difficulty counter keeps moving.
pub const LOCAL_MIN_DIFFICULTY: f64 = 0.0001;
