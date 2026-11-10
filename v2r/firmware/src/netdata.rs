//! The network figures of the clock, global hashrate and price screens, from
//! the same APIs as the C++ firmware's `monitor.cpp`, and only while a screen
//! that shows them is on.
//!
//! Like the C++ firmware's `HTTPClient`, this does not check the servers'
//! certificates. TLS runs in software, so it never touches the SHA
//! accelerator that core 0 mines with.

use core::cell::RefCell;
use core::fmt::Write;
use core::sync::atomic::{AtomicU8, Ordering};

use embassy_net::Stack;
use embassy_net::dns::DnsSocket;
use embassy_net::tcp::client::{TcpClient, TcpClientState};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use heapless::String;
use log::{info, warn};
use picominer_core::json;
use reqwless::client::{HttpClient, TlsConfig, TlsVerify};
use reqwless::request::{Method, RequestBuilder};
use static_cell::ConstStaticCell;

const BTC_API: &str = "https://api.coingecko.com/api/v3/simple/price?ids=bitcoin&vs_currencies=usd";
const HEIGHT_API: &str = "https://mempool.space/api/blocks/tip/height";
const GLOBAL_HASH_API: &str = "https://mempool.space/api/v1/mining/hashrate/3d";
const FEES_API: &str = "https://mempool.space/api/v1/fees/recommended";

/// `UPDATE_BTC_min`, `UPDATE_Height_min` and `UPDATE_Global_min`.
const BTC_PERIOD: Duration = Duration::from_secs(60);
const HEIGHT_PERIOD: Duration = Duration::from_secs(120);
const GLOBAL_PERIOD: Duration = Duration::from_secs(120);
/// After a failure. The C++ firmware tries again at every refresh.
const RETRY: Duration = Duration::from_secs(30);
const TIMEOUT: Duration = Duration::from_secs(10);

pub const WANT_PRICE: u8 = 1;
pub const WANT_HEIGHT: u8 = 2;
pub const WANT_GLOBAL: u8 = 4;

/// What the screen on display needs, set at each refresh.
static WANTED: AtomicU8 = AtomicU8::new(0);

#[derive(Clone)]
pub struct NetData {
    pub btc_price: u32,
    pub block_height: String<12>,
    pub global_hash: String<16>,
    pub difficulty: String<16>,
    pub half_hour_fee: i32,
}

static DATA: Mutex<CriticalSectionRawMutex, RefCell<Option<NetData>>> = Mutex::new(RefCell::new(None));

fn with_data<R>(change: impl FnOnce(&mut NetData) -> R) -> R {
    DATA.lock(|data| {
        let mut data = data.borrow_mut();
        let data = data.get_or_insert_with(|| NetData {
            btc_price: 0,
            // The C++ firmware's `current_block` before its first answer.
            block_height: String::try_from("793261").unwrap_or_default(),
            global_hash: String::new(),
            difficulty: String::new(),
            half_hour_fee: 0,
        });
        change(data)
    })
}

pub fn get() -> NetData {
    with_data(|data| data.clone())
}

pub fn want(what: u8) {
    WANTED.store(what, Ordering::Relaxed);
}

/// `currentHashrate` in H/s, printed like `String(float)` and cut 21
/// characters short: whole exahashes.
fn global_hash(value: f64) -> Option<String<16>> {
    let value = f64::from(value as f32);
    let mut out = String::new();
    if value >= 1e18 {
        write!(out, "{}", (value / 1e18) as u64).ok()?;
    }
    Some(out)
}

/// `currentDifficulty` cut to hundredths of a tera, `148.23T`.
fn difficulty(value: f64) -> Option<String<16>> {
    let value = f64::from(value as f32);
    let mut out = String::new();
    if value >= 1e10 {
        let hundredths = (value / 1e10) as u64;
        write!(out, "{}.{:02}T", hundredths / 100, hundredths % 100).ok()?;
    }
    Some(out)
}

static TLS_READ: ConstStaticCell<[u8; 16640]> = ConstStaticCell::new([0; 16640]);
static TLS_WRITE: ConstStaticCell<[u8; 4096]> = ConstStaticCell::new([0; 4096]);
static BODY: ConstStaticCell<[u8; 4096]> = ConstStaticCell::new([0; 4096]);
static TCP: ConstStaticCell<TcpClientState<1, 4096, 4096>> = ConstStaticCell::new(TcpClientState::new());

struct Fetcher {
    stack: Stack<'static>,
    tcp: &'static TcpClientState<1, 4096, 4096>,
    tls_read: &'static mut [u8; 16640],
    tls_write: &'static mut [u8; 4096],
    body: &'static mut [u8; 4096],
    seed: u64,
}

impl Fetcher {
    /// The body of a GET, as text.
    async fn get(&mut self, url: &str) -> Result<&str, &'static str> {
        let tcp = TcpClient::new(self.stack, self.tcp);
        let dns = DnsSocket::new(self.stack);
        self.seed = self.seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        let tls = TlsConfig::new(self.seed, self.tls_read, self.tls_write, TlsVerify::None);
        let mut client = HttpClient::new_with_tls(&tcp, &dns, tls);
        // CoinGecko answers 403 to a request without a User-Agent; Arduino's
        // HTTPClient always sends one.
        let mut request = client
            .request(Method::GET, url)
            .await
            .map_err(|_| "connect failed")?
            .headers(&[("User-Agent", crate::config::AGENT)]);
        let response = request.send(self.body).await.map_err(|_| "request failed")?;
        if !response.status.is_successful() {
            return Err("HTTP error");
        }
        let body = response.body().read_to_end().await.map_err(|_| "read failed")?;
        core::str::from_utf8(body).map_err(|_| "not text")
    }

    async fn price(&mut self) -> Result<(), &'static str> {
        let text = self.get(BTC_API).await?;
        let (value, _) = json::parse(text).ok_or("bad JSON")?;
        let usd = value.get("bitcoin").and_then(|b| b.get("usd")).and_then(|u| u.as_f64()).ok_or("no price")?;
        with_data(|data| data.btc_price = usd as u32);
        Ok(())
    }

    async fn height(&mut self) -> Result<(), &'static str> {
        let text = self.get(HEIGHT_API).await?.trim();
        let height = String::try_from(text).map_err(|_| "height too long")?;
        with_data(|data| data.block_height = height);
        Ok(())
    }

    async fn global(&mut self) -> Result<(), &'static str> {
        let text = self.get(GLOBAL_HASH_API).await?;
        let (value, _) = json::parse(text).ok_or("bad JSON")?;
        let hash = value.get("currentHashrate").and_then(|v| v.as_f64()).and_then(global_hash);
        let diff = value.get("currentDifficulty").and_then(|v| v.as_f64()).and_then(difficulty);
        with_data(|data| {
            if let Some(hash) = hash.filter(|h| !h.is_empty()) {
                data.global_hash = hash;
            }
            if let Some(diff) = diff.filter(|d| !d.is_empty()) {
                data.difficulty = diff;
            }
        });
        let text = self.get(FEES_API).await?;
        let (value, _) = json::parse(text).ok_or("bad JSON")?;
        if let Some(fee) = value.get("halfHourFee").and_then(|v| v.as_f64()) {
            with_data(|data| data.half_hour_fee = fee as i32);
        }
        Ok(())
    }
}

/// When each figure is next due.
struct Due {
    price: Instant,
    height: Instant,
    global: Instant,
}

#[embassy_executor::task]
pub async fn run(stack: Stack<'static>, seed: u64) -> ! {
    let mut fetcher = Fetcher {
        stack,
        tcp: TCP.take(),
        tls_read: TLS_READ.take(),
        tls_write: TLS_WRITE.take(),
        body: BODY.take(),
        seed,
    };
    let now = Instant::now();
    let mut due = Due { price: now, height: now, global: now };
    loop {
        Timer::after_secs(1).await;
        let wanted = WANTED.load(Ordering::Relaxed);
        if wanted == 0 || !stack.is_config_up() {
            continue;
        }
        let now = Instant::now();
        if wanted & WANT_PRICE != 0 && now >= due.price {
            let result = with_timeout(TIMEOUT, fetcher.price()).await.unwrap_or(Err("timed out"));
            due.price = now + if result.is_ok() { BTC_PERIOD } else { RETRY };
            report("BTC price", result);
        }
        if wanted & WANT_HEIGHT != 0 && now >= due.height {
            let result = with_timeout(TIMEOUT, fetcher.height()).await.unwrap_or(Err("timed out"));
            due.height = now + if result.is_ok() { HEIGHT_PERIOD } else { RETRY };
            report("block height", result);
        }
        if wanted & WANT_GLOBAL != 0 && now >= due.global {
            let result = with_timeout(TIMEOUT * 2, fetcher.global()).await.unwrap_or(Err("timed out"));
            due.global = now + if result.is_ok() { GLOBAL_PERIOD } else { RETRY };
            report("global data", result);
        }
    }
}

fn report(what: &str, result: Result<(), &'static str>) {
    match result {
        Ok(()) => info!("{} updated", what),
        Err(reason) => warn!("{} failed: {}", what, reason),
    }
}
