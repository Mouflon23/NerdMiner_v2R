//! Settings baked in at build time from `v2r/.env`, plus tuning constants.

pub const WIFI_SSID: &str = env!("V2R_WIFI_SSID");
pub const WIFI_PASSWORD: &str = env!("V2R_WIFI_PASSWORD");
pub const BTC_ADDRESS: &str = env!("V2R_BTC_ADDRESS");
pub const POOL_HOST: &str = env!("V2R_POOL_HOST");
pub const POOL_PORT: &str = env!("V2R_POOL_PORT");
pub const WORKER: &str = env!("V2R_WORKER");
const TIMEZONE: &str = env!("V2R_TIMEZONE");

pub const AGENT: &str = concat!("NerdMinerV2R/", env!("CARGO_PKG_VERSION"));

/// About one share every 20 s at 350 kH/s: a difficulty-1 share takes 2³²
/// hashes.
pub const SUGGESTED_DIFFICULTY: f64 = 0.0015;

/// Hits below this are not worth reporting, whatever the pool asks for. Set well
/// under any pool difficulty so the best-difficulty counter keeps moving.
pub const LOCAL_MIN_DIFFICULTY: f64 = 0.0001;

pub fn port() -> u16 {
    POOL_PORT.parse().unwrap_or(3333)
}

/// Hours from UTC, as `Settings.Timezone` in the C++ firmware.
pub fn timezone() -> i32 {
    TIMEZONE.parse().unwrap_or(2)
}
