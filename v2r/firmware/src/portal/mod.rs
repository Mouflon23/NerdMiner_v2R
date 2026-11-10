//! The configuration portal, `wManager.cpp` with WiFiManager v2.0.17 in it.
//!
//! The board opens the `NerdMinerAP` access point (password `MineYourCoins`),
//! answers every DNS name with its own address so that phones open the portal
//! by themselves, and serves WiFiManager's pages, with NerdMiner's settings
//! under the Wi-Fi form. Saving writes the settings to flash and restarts.
//!
//! It opens, as in the C++ firmware, when no settings are stored, when the
//! second button is held at boot, or when the stored network does not answer
//! within 40 s. With no web request for 180 s, the board restarts.
//!
//! WiFiManager's firmware update page is not here yet: it needs a partition
//! table with two app slots.

mod dhcp;
mod dns;
mod http;
mod strings;

use alloc::format;
use alloc::string::String;
use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_futures::select::{Either3, select3};
use embassy_net::tcp::TcpSocket;
use embassy_net::{Ipv4Address, Ipv4Cidr, Stack, StackResources, StaticConfigV4};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use esp_radio::wifi::ap::AccessPointConfig;
use esp_radio::wifi::scan::ScanConfig;
use esp_radio::wifi::{AuthenticationMethod, AuthenticationMethodConfig, Config, Interface, WifiController};
use heapless::Vec;
use log::{info, warn};
use static_cell::StaticCell;

use crate::pool::JoinFailure;
use crate::settings::{self, Settings, bounded, clean};
use crate::stats::{self, Status};
use strings::*;

pub const AP_ADDRESS: Ipv4Address = Ipv4Address::new(192, 168, 4, 1);
const AP_HOST: &str = "192.168.4.1";

/// `setConfigPortalTimeout(180)`, counted from the last web request.
const TIMEOUT: Duration = Duration::from_secs(180);
/// WiFiManager reuses a scan for this long.
const SCAN_CACHE: Duration = Duration::from_secs(30);
const MAX_NETWORKS: usize = 32;
const HTTP_TASKS: usize = 3;

/// `WM_VERSION_STR`, for the info page.
const WM_VERSION: &str = "v2.0.17";

static RESOURCES: StaticCell<StackResources<{ HTTP_TASKS + 3 }>> = StaticCell::new();
static INFO: StaticCell<Info> = StaticCell::new();

#[derive(Clone)]
struct Network {
    ssid: heapless::String<32>,
    rssi: i8,
    open: bool,
}

struct Scan {
    networks: Vec<Network, MAX_NETWORKS>,
    at: Option<Instant>,
}

static SCAN: Mutex<CriticalSectionRawMutex, RefCell<Scan>> =
    Mutex::new(RefCell::new(Scan { networks: Vec::new(), at: None }));
static SCAN_WANTED: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static SCAN_DONE: Signal<CriticalSectionRawMutex, ()> = Signal::new();
/// A restart, after this many milliseconds for the page to get out.
static RESTART: Signal<CriticalSectionRawMutex, u64> = Signal::new();
/// When the last web request came, in seconds since boot.
static LAST_REQUEST: AtomicU32 = AtomicU32::new(0);

/// What the pages say about the board.
struct Info {
    ap_name: heapless::String<32>,
    ap_mac: [u8; 6],
    station_mac: [u8; 6],
    failure: JoinFailure,
}

fn mac(bytes: &[u8; 6]) -> String {
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]
    )
}

/// WiFiManager's `htmlEntities`.
fn entities(text: &str, whitespace: bool) -> String {
    let text = text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('\'', "&#39;");
    if whitespace { text.replace(' ', "&#160;") } else { text }
}

/// `getHTTPHead`. The body class token stays as WiFiManager leaves it.
fn head(title: &str) -> String {
    let mut page = HTTP_HEAD_START.replace("{v}", title);
    page.push_str(HTTP_SCRIPT);
    page.push_str(HTTP_STYLE);
    page.push_str(HTTP_HEAD_END);
    page
}

/// `reportStatus`: the board is never on the stored network here.
fn status(page: &mut String, info: &Info, settings: &Settings) {
    if settings.wifi_ssid.is_empty() {
        page.push_str(HTTP_STATUS_NONE);
        return;
    }
    let (class, reason) = match info.failure {
        JoinFailure::None => ("", ""),
        JoinFailure::WrongPassword => ("D", HTTP_STATUS_OFFPW),
        JoinFailure::NotFound => ("D", HTTP_STATUS_OFFNOAP),
        JoinFailure::Failed => ("D", HTTP_STATUS_OFFFAIL),
    };
    page.push_str(
        &HTTP_STATUS_OFF
            .replace("{v}", &entities(&settings.wifi_ssid, false))
            .replace("{c}", class)
            .replace("{r}", reason),
    );
}

fn root_page(info: &Info, settings: &Settings) -> String {
    let mut page = head(S_BRAND);
    page.push_str(&HTTP_ROOT_MAIN.replace("{t}", S_BRAND).replace("{v}", &info.ap_name));
    // `_menuIdsDefault` without the update page: wifi, info, exit, sep.
    for item in [MENU_WIFI, MENU_INFO, MENU_EXIT, MENU_SEP] {
        page.push_str(item);
    }
    status(&mut page, info, settings);
    page.push_str(HTTP_END);
    page
}

/// `getScanItemOut`: by signal, without duplicates or hidden networks.
fn scan_items(page: &mut String) {
    let mut networks = SCAN.lock(|scan| scan.borrow().networks.clone());
    networks.sort_unstable_by(|a, b| b.rssi.cmp(&a.rssi));
    let mut shown: Vec<heapless::String<32>, MAX_NETWORKS> = Vec::new();
    // The percentage hidden and the icons shown, `_scanDispOptions` false.
    let item = HTTP_ITEM.replace("{qp}", HTTP_ITEM_QP).replace("{h}", "h").replace("{qi}", HTTP_ITEM_QI).replace("{h}", "");
    let mut any = false;
    for network in &networks {
        if network.ssid.is_empty() || shown.contains(&network.ssid) {
            continue;
        }
        let _ = shown.push(network.ssid.clone());
        let quality = match network.rssi {
            ..=-100 => 0,
            -50.. => 100,
            rssi => 2 * (i32::from(rssi) + 100),
        };
        let line = item
            .replace("{V}", &entities(&network.ssid, false))
            .replace("{v}", &entities(&network.ssid, true))
            .replace("{r}", &format!("{quality}"))
            .replace("{q}", &format!("{}", quality * 3 / 100 + 1))
            .replace("{i}", if network.open { "" } else { "l" });
        page.push_str(&line);
        any = true;
    }
    if any {
        page.push_str(HTTP_BR);
    } else {
        page.push_str(S_NONETWORKS);
        page.push_str("<br/><br/>");
    }
}

/// One `WiFiManagerParameter`, label before or after.
fn param(page: &mut String, id: &str, label: &str, value: &str, length: usize, custom: &str, label_after: bool) {
    let input = HTTP_FORM_PARAM
        .replace("{i}", id)
        .replace("{n}", id)
        .replace("{l}", &format!("{length}"))
        .replace("{v}", &entities(value, false))
        .replace("{c}", custom);
    let label = HTTP_FORM_LABEL.replace("{i}", id).replace("{t}", label);
    if label_after {
        page.push_str(&input);
        page.push_str(&label);
    } else {
        page.push_str(&label);
        page.push_str(&input);
    }
}

/// The parameters `init_WifiManager` adds, in its order.
fn params(page: &mut String, settings: &Settings) {
    param(page, "Poolurl", "Pool url", &settings.pool_url, 80, "", false);
    param(page, "Poolport", "Pool port", &format!("{}", settings.pool_port), 7, "", false);
    param(page, "Poolpassword - Optional", "Pool password", &settings.pool_password, 80, "", false);
    param(page, "btcAddress", "Your BTC address", &settings.btc_wallet, 80, "", false);
    param(page, "TimeZone", "TimeZone fromUTC (-12/+12)", &format!("{}", settings.timezone), 3, "", false);
    page.push_str("<hr><br><label style=\"font-weight: bold;margin-bottom: 25px;display: inline-block;\">Features</label>");
    let checkbox = if settings.save_stats { "type=\"checkbox\" checked" } else { "type=\"checkbox\"" };
    param(page, "SaveStatsToNVS", "Save mining statistics to flash memory.", "T", 2, checkbox, true);
}

/// `doParamSave`, then NerdMiner's copy into `Settings`. A field the form
/// left out keeps its value, but for the checkbox, which a browser leaves out
/// when it is cleared.
fn save_params(request: &http::Request, settings: &mut Settings) {
    if let Some(url) = request.arg("Poolurl") {
        settings.pool_url = clean(url.trim());
    }
    if let Some(port) = request.arg("Poolport").and_then(|port| port.trim().parse().ok()) {
        settings.pool_port = port;
    }
    if let Some(password) = request.arg("Poolpassword - Optional") {
        settings.pool_password = clean(&password);
    }
    if let Some(wallet) = request.arg("btcAddress") {
        settings.btc_wallet = clean(wallet.trim());
    }
    if let Some(zone) = request.arg("TimeZone").and_then(|zone| zone.trim().parse().ok()) {
        settings.timezone = zone;
    }
    settings.save_stats = request.arg("SaveStatsToNVS").is_some_and(|value| value.starts_with('T'));
}

fn wifi_page(info: &Info, settings: &Settings, scan: bool) -> String {
    let mut page = head(S_TITLEWIFI);
    if scan {
        scan_items(&mut page);
    }
    page.push_str(&HTTP_FORM_START.replace("{v}", "wifisave"));
    let placeholder = if settings.wifi_password.is_empty() { "" } else { S_PASSPH };
    page.push_str(&HTTP_FORM_WIFI.replace("{v}", &settings.wifi_ssid).replace("{p}", placeholder));
    page.push_str(HTTP_FORM_PARAM_HEAD);
    params(&mut page, settings);
    page.push_str(HTTP_FORM_END);
    page.push_str(HTTP_SCAN_LINK);
    status(&mut page, info, settings);
    page.push_str(HTTP_END);
    page
}

fn param_page(info: &Info, settings: &Settings) -> String {
    let mut page = head(S_TITLEPARAM);
    page.push_str(&HTTP_FORM_START.replace("{v}", "paramsave"));
    params(&mut page, settings);
    page.push_str(HTTP_FORM_END);
    status(&mut page, info, settings);
    page.push_str(HTTP_END);
    page
}

fn info_page(info: &Info, settings: &Settings) -> String {
    let item = |template: &str, one: &str| template.replace("{1}", one);
    let mut page = head(S_TITLEINFO);
    status(&mut page, info, settings);
    let uptime = Instant::now().as_secs();
    let revision = esp_hal::efuse::chip_revision();
    let station_mac = info.station_mac;
    let chip_id = u32::from_le_bytes([station_mac[0], station_mac[1], station_mac[2], station_mac[3]]);
    let reset = format!("{:?}", esp_hal::system::reset_reason());
    let celsius = crate::temperature::read();
    let yes_no = |yes: bool| if yes { S_Y } else { S_N };
    page.push_str(HTTP_INFO_ESPHEAD);
    page.push_str(&HTTP_INFO_UPTIME.replace("{1}", &format!("{}", uptime / 60)).replace("{2}", &format!("{}", uptime % 60)));
    page.push_str(&item(HTTP_INFO_CHIPID, &format!("{chip_id:x}")));
    page.push_str(&item(HTTP_INFO_CHIPREV, &format!("{}", u32::from(revision.major) * 100 + u32::from(revision.minor))));
    page.push_str(&item(HTTP_INFO_IDESIZE, &format!("{}", settings::flash_size())));
    page.push_str(&item(HTTP_INFO_CPUFREQ, "240"));
    page.push_str(&item(HTTP_INFO_FREEHEAP, &format!("{}", esp_alloc::HEAP.free())));
    page.push_str(&HTTP_INFO_LASTRESET.replace("{1}", &reset).replace("{2}", &reset));
    page.push_str(
        &HTTP_INFO_TEMP.replace("{1}", &format!("{celsius:.2}")).replace("{2}", &format!("{:.2}", celsius * 1.8 + 32.0)),
    );
    page.push_str(HTTP_INFO_WIFIHEAD);
    page.push_str(&item(HTTP_INFO_CONX, yes_no(false)));
    page.push_str(&item(HTTP_INFO_STASSID, &entities(&settings.wifi_ssid, false)));
    page.push_str(&item(HTTP_INFO_STAIP, "0.0.0.0"));
    page.push_str(&item(HTTP_INFO_STAMAC, &mac(&info.station_mac)));
    page.push_str(&item(HTTP_INFO_APSSID, &entities(&info.ap_name, false)));
    page.push_str(&item(HTTP_INFO_APIP, AP_HOST));
    page.push_str(&item(HTTP_INFO_APMAC, &mac(&info.ap_mac)));
    page.push_str(&item(HTTP_INFO_APHOST, crate::config::AGENT));
    page.push_str(&item(HTTP_INFO_APBSSID, &mac(&info.ap_mac)));
    page.push_str("</dl>");
    page.push_str("<h3>About</h3><hr><dl>");
    page.push_str(&item(HTTP_INFO_ABOUTVER, WM_VERSION));
    page.push_str(&item(HTTP_INFO_ABOUTDATE, crate::config::AGENT));
    page.push_str("</dl>");
    page.push_str(HTTP_ERASEBTN);
    page.push_str(HTTP_HELP);
    page.push_str(HTTP_END);
    page
}

fn simple_page(title: &str, text: &str, end: bool) -> String {
    let mut page = head(title);
    page.push_str(text);
    if end {
        page.push_str(HTTP_END);
    }
    page
}

/// Waits for the scan the portal loop runs, if the cached one is too old.
async fn fresh_scan(force: bool) {
    let stale = SCAN.lock(|scan| scan.borrow().at.is_none_or(|at| at.elapsed() > SCAN_CACHE));
    if force || stale {
        SCAN_DONE.reset();
        SCAN_WANTED.signal(());
        let _ = embassy_time::with_timeout(Duration::from_secs(10), SCAN_DONE.wait()).await;
    }
}

const NO_CACHE: [(&str, &str); 3] =
    [("Cache-Control", "no-cache, no-store, must-revalidate"), ("Pragma", "no-cache"), ("Expires", "-1")];

async fn html(socket: &mut TcpSocket<'_>, page: String) -> Result<(), ()> {
    http::respond(socket, "200 OK", "text/html", &[], &page).await
}

/// `captivePortal()`: a request for any other host goes to the portal.
async fn redirect(socket: &mut TcpSocket<'_>) -> Result<(), ()> {
    http::respond(socket, "302 Found", "text/plain", &[("Location", "http://192.168.4.1")], "").await
}

async fn handle(socket: &mut TcpSocket<'_>, info: &Info) -> Result<(), ()> {
    let request = http::read(socket).await.ok_or(())?;
    LAST_REQUEST.store(Instant::now().as_secs() as u32, Ordering::Relaxed);
    let settings = settings::get();
    let captive = request.host() != AP_HOST;
    info!("portal: {} {}", request.method(), request.path());
    match request.path() {
        "/" if captive => redirect(socket).await,
        "/" => html(socket, root_page(info, &settings)).await,
        "/wifi" | "/0wifi" => {
            let scan = request.path() == "/wifi";
            if scan {
                fresh_scan(request.has_arg("refresh")).await;
            }
            html(socket, wifi_page(info, &settings, scan)).await
        }
        "/wifisave" => {
            let mut new = settings.clone();
            let mut ssid = request.arg("s").unwrap_or_default();
            let password = request.arg("p").unwrap_or_default();
            if ssid.is_empty() && !password.is_empty() {
                ssid = String::from(settings.wifi_ssid.as_str());
            }
            save_params(&request, &mut new);
            let page = if ssid.is_empty() {
                simple_page(S_TITLEWIFISETTINGS, HTTP_PARAMSAVED, false)
            } else {
                new.wifi_ssid = bounded(&ssid);
                new.wifi_password = bounded(&password);
                simple_page(S_TITLEWIFISAVED, HTTP_SAVED, false)
            };
            let mut page = page;
            page.push_str(HTTP_END);
            let sent = http::respond(socket, "200 OK", "text/html", &[("Access-Control-Allow-Origin", "*")], &page).await;
            if settings::save(&new) {
                // NerdMiner's `delay(3*SECOND_MS)` before `ESP.restart()`.
                RESTART.signal(3000);
            }
            sent
        }
        "/param" => html(socket, param_page(info, &settings)).await,
        "/paramsave" => {
            let mut new = settings.clone();
            save_params(&request, &mut new);
            settings::save(&new);
            let mut page = simple_page(S_TITLEPARAMSAVED, HTTP_PARAMSAVED, false);
            page.push_str(HTTP_END);
            html(socket, page).await
        }
        "/info" => html(socket, info_page(info, &settings)).await,
        "/exit" => {
            let page = simple_page(S_TITLEEXIT, S_EXITING, false);
            let sent = http::respond(socket, "200 OK", "text/html", &NO_CACHE[..1], &page).await;
            RESTART.signal(2000);
            sent
        }
        "/restart" => {
            let sent = html(socket, simple_page(S_TITLERESET, S_RESETTING, true)).await;
            RESTART.signal(1000);
            sent
        }
        "/erase" => {
            let mut new = settings.clone();
            new.wifi_ssid.clear();
            new.wifi_password.clear();
            let erased = settings::save(&new);
            let sent = html(socket, simple_page(S_TITLEERASE, if erased { S_RESETTING } else { S_ERROR }, true)).await;
            if erased {
                RESTART.signal(2000);
            }
            sent
        }
        "/close" => html(socket, simple_page(S_TITLECLOSE, S_CLOSING, false)).await,
        _ if captive => redirect(socket).await,
        _ => http::respond(socket, "404 Not Found", "text/plain", &NO_CACHE, S_NOTFOUND).await,
    }
}

#[embassy_executor::task(pool_size = HTTP_TASKS)]
async fn serve_http(stack: Stack<'static>, info: &'static Info) -> ! {
    let mut rx = [0u8; 1536];
    let mut tx = [0u8; 2048];
    loop {
        let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
        socket.set_timeout(Some(Duration::from_secs(10)));
        if socket.accept(80).await.is_ok() {
            let _ = handle(&mut socket, info).await;
            socket.close();
            let _ = embassy_time::with_timeout(Duration::from_secs(2), socket.flush()).await;
        }
        socket.abort();
    }
}

async fn scan(controller: &mut WifiController<'static>) {
    match controller.scan_async(&ScanConfig::default()).await {
        Ok(found) => {
            let mut networks = Vec::new();
            for ap in found {
                let open = matches!(ap.auth_method, None | Some(AuthenticationMethod::None));
                let network = Network { ssid: bounded(ap.ssid.as_str()), rssi: ap.signal_strength, open };
                if networks.push(network).is_err() {
                    break;
                }
            }
            info!("portal: {} networks", networks.len());
            SCAN.lock(|scan| *scan.borrow_mut() = Scan { networks, at: Some(Instant::now()) });
        }
        Err(error) => warn!("portal: scan failed: {:?}", error),
    }
    SCAN_DONE.signal(());
}

/// Opens the portal and never returns: every way out restarts the board.
pub async fn run(spawner: Spawner, mut controller: WifiController<'static>, failure: JoinFailure, seed: u64) -> ! {
    stats::update(|stats| stats.status = Status::Portal);
    let settings = settings::get();
    let ap_name = settings::ap_name();
    let ap = AccessPointConfig::default()
        .with_ssid(ap_name.as_str().try_into().expect("AP name fits"))
        .with_authentication(AuthenticationMethodConfig::Wpa2Personal(
            settings::AP_PASSWORD.try_into().expect("AP password fits"),
        ));
    controller
        .set_config(&Config::AccessPointStation(crate::station_config(&settings), ap))
        .expect("access point configuration");
    info!("portal: access point {} at {}", ap_name, AP_HOST);

    let interface = Interface::access_point();
    let info = INFO.init(Info {
        ap_name,
        ap_mac: interface.mac_address(),
        station_mac: esp_hal::efuse::base_mac_address().as_bytes().try_into().unwrap_or_default(),
        failure,
    });
    let config = embassy_net::Config::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(AP_ADDRESS, 24),
        gateway: Some(AP_ADDRESS),
        dns_servers: Vec::new(),
    });
    let (stack, runner) = embassy_net::new(interface, config, RESOURCES.init(StackResources::new()), seed);
    spawner.spawn(crate::net(runner).expect("access point net task"));
    spawner.spawn(dhcp::serve(stack).expect("DHCP task"));
    spawner.spawn(dns::serve(stack).expect("DNS task"));
    for _ in 0..HTTP_TASKS {
        spawner.spawn(serve_http(stack, info).expect("HTTP task"));
    }

    scan(&mut controller).await;
    LAST_REQUEST.store(Instant::now().as_secs() as u32, Ordering::Relaxed);
    loop {
        match select3(SCAN_WANTED.wait(), RESTART.wait(), Timer::after_secs(1)).await {
            Either3::First(()) => scan(&mut controller).await,
            Either3::Second(delay) => {
                info!("portal: restarting");
                Timer::after_millis(delay).await;
                esp_hal::system::software_reset();
            }
            Either3::Third(()) => {
                let idle = (Instant::now().as_secs() as u32).saturating_sub(LAST_REQUEST.load(Ordering::Relaxed));
                if u64::from(idle) >= TIMEOUT.as_secs() {
                    info!("portal: no request for {} s, restarting", idle);
                    esp_hal::system::software_reset();
                }
            }
        }
    }
}
