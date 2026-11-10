//! The settings the configuration portal edits, kept in flash.
//!
//! The C++ firmware keeps its Wi-Fi credentials where the IDF Wi-Fi driver
//! puts them and the rest in `/config.json` on SPIFFS. Here they all live in
//! one record in the first sector of the `nvs` partition, which nothing else
//! uses: a magic word, the length and a checksum, then one `key=value` line per
//! setting, with the C++ JSON keys.
//!
//! The values in `v2r/.env` are only what the portal's form starts with, where
//! the C++ firmware has its compiled-in defaults.

use core::cell::RefCell;
use core::fmt::Write;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use esp_bootloader_esp_idf::partitions::{self, DataPartitionSubType, PartitionType};
use esp_storage::FlashStorage;
use heapless::String;
use log::{info, warn};

/// `DEFAULT_SSID` and `DEFAULT_WIFIPW`: the portal's access point.
pub const AP_SSID: &str = "NerdMinerAP";
pub const AP_PASSWORD: &str = "MineYourCoins";

const MAGIC: u32 = u32::from_le_bytes(*b"V2RS");
const HEADER: usize = 12;
const SECTOR: u32 = FlashStorage::SECTOR_SIZE;
const RECORD_MAX: usize = 1024;
/// The settings, then the statistics.
const SETTINGS_SECTOR: u32 = 0;
const STATS_SECTOR: u32 = 1;
const SECTORS: u32 = 2;

/// Where `readCustomAPName` looks for an access point name a web flasher left.
const WEBFLASHER_CONFIG: u32 = 0x3F_0000;
const WEBFLASHER_MARKER: &[u8] = b"WEBFLASHER_CONFIG:";

/// `TSettings`, but for the screen options of other boards.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub wifi_ssid: String<32>,
    pub wifi_password: String<64>,
    pub pool_url: String<80>,
    pub pool_port: u16,
    pub pool_password: String<80>,
    /// What the pool takes as the user name: an address, maybe `.worker`.
    pub btc_wallet: String<80>,
    /// Hours from UTC.
    pub timezone: i32,
    pub save_stats: bool,
}

/// Copies what fits of `text`: the form limits every field anyway.
pub fn bounded<const N: usize>(text: &str) -> String<N> {
    let mut out = String::new();
    for c in text.chars() {
        if out.push(c).is_err() {
            break;
        }
    }
    out
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            wifi_ssid: bounded(env!("V2R_WIFI_SSID")),
            wifi_password: bounded(env!("V2R_WIFI_PASSWORD")),
            pool_url: bounded(env!("V2R_POOL_HOST")),
            pool_port: env!("V2R_POOL_PORT").parse().unwrap_or(3333),
            pool_password: bounded(env!("V2R_POOL_PASSWORD")),
            btc_wallet: bounded(env!("V2R_BTC_WALLET")),
            timezone: env!("V2R_TIMEZONE").parse().unwrap_or(2),
            save_stats: false,
        }
    }
}

impl Settings {
    fn encode(&self) -> String<RECORD_MAX> {
        let mut out = String::new();
        let _ = write!(
            out,
            "SSID={}\nWifiPW={}\npoolString={}\nportNumber={}\npoolPassword={}\nbtcString={}\ngmtZone={}\nsaveStatsToNVS={}\n",
            self.wifi_ssid,
            self.wifi_password,
            self.pool_url,
            self.pool_port,
            self.pool_password,
            self.btc_wallet,
            self.timezone,
            u8::from(self.save_stats),
        );
        out
    }

    fn decode(text: &str) -> Self {
        let mut settings = Self::default();
        for (key, value) in text.lines().filter_map(|line| line.split_once('=')) {
            match key {
                "SSID" => settings.wifi_ssid = bounded(value),
                "WifiPW" => settings.wifi_password = bounded(value),
                "poolString" => settings.pool_url = bounded(value),
                "portNumber" => settings.pool_port = value.parse().unwrap_or(settings.pool_port),
                "poolPassword" => settings.pool_password = bounded(value),
                "btcString" => settings.btc_wallet = bounded(value),
                "gmtZone" => settings.timezone = value.parse().unwrap_or(settings.timezone),
                "saveStatsToNVS" => settings.save_stats = value == "1",
                _ => {}
            }
        }
        settings
    }
}

/// Keeps a line break out of a value, which would end it early.
pub fn clean<const N: usize>(text: &str) -> String<N> {
    let mut out = String::new();
    for c in text.chars().filter(|c| !c.is_control()) {
        if out.push(c).is_err() {
            break;
        }
    }
    out
}

fn checksum(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5, |hash, &byte| (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193))
}

struct Store {
    flash: FlashStorage<'static>,
    /// Start of the settings sector, in the `nvs` partition.
    offset: u32,
}

struct State {
    store: Option<Store>,
    settings: Settings,
    stored: bool,
    ap_name: String<32>,
    /// The statistics record read at boot, for [`take_stats`].
    stats: Option<String<RECORD_MAX>>,
}

static STATE: Mutex<CriticalSectionRawMutex, RefCell<Option<State>>> = Mutex::new(RefCell::new(None));

fn nvs_offset(flash: &mut FlashStorage<'static>) -> Option<u32> {
    let mut buffer = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
    let table = partitions::read_partition_table(flash, &mut buffer).ok()?;
    let entry = table.find_partition(PartitionType::Data(DataPartitionSubType::Nvs)).ok()??;
    (entry.len() >= SECTORS * SECTOR).then(|| entry.offset())
}

/// The text of the record in sector `index` of the partition, if valid.
fn read_record(flash: &mut FlashStorage<'static>, offset: u32, index: u32) -> Option<String<RECORD_MAX>> {
    let offset = offset + index * SECTOR;
    let mut header = [0u8; HEADER];
    flash.read(offset, &mut header).ok()?;
    let word = |at: usize| u32::from_le_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]]);
    let length = word(4) as usize;
    if word(0) != MAGIC || length > RECORD_MAX {
        return None;
    }
    let mut body = [0u8; RECORD_MAX];
    flash.read(offset + HEADER as u32, &mut body[..length]).ok()?;
    if checksum(&body[..length]) != word(8) {
        warn!("flash record {}: bad checksum", index);
        return None;
    }
    Some(bounded(core::str::from_utf8(&body[..length]).ok()?))
}

/// Rewrites sector `index` with `text`. Core 0 is parked meanwhile.
fn write_record(store: &mut Store, index: u32, text: &str) -> Result<(), esp_storage::FlashStorageError> {
    let mut record = [0xffu8; HEADER + RECORD_MAX];
    let text = &text.as_bytes()[..text.len().min(RECORD_MAX)];
    record[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    record[4..8].copy_from_slice(&(text.len() as u32).to_le_bytes());
    record[8..12].copy_from_slice(&checksum(text).to_le_bytes());
    record[HEADER..HEADER + text.len()].copy_from_slice(text);
    // Whole words, as the flash writes them.
    let length = (HEADER + text.len()).next_multiple_of(4);
    let offset = store.offset + index * SECTOR;
    store.flash.erase(offset, offset + SECTOR)?;
    store.flash.write_nor(offset, &record[..length])
}

/// The name a web flasher may have left for the access point.
fn custom_ap_name(flash: &mut FlashStorage<'static>) -> Option<String<32>> {
    let mut data = [0u8; 128];
    flash.read(WEBFLASHER_CONFIG, &mut data).ok()?;
    let json = data.strip_prefix(WEBFLASHER_MARKER)?;
    let end = json.iter().position(|&b| b == 0 || b == 0xff).unwrap_or(json.len());
    let json = core::str::from_utf8(&json[..end]).ok()?;
    let rest = &json[json.find("\"apname\"")? + 8..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    let name = rest[..rest.find('"')?].trim();
    (!name.is_empty() && name.len() < 32).then(|| bounded(name))
}

/// Reads the settings, once, before core 1 starts.
pub fn init(flash: esp_hal::peripherals::FLASH<'static>) {
    let mut flash = FlashStorage::new(flash).multicore_auto_park();
    let offset = nvs_offset(&mut flash);
    let stored = offset.and_then(|offset| read_record(&mut flash, offset, SETTINGS_SECTOR));
    let stored = stored.map(|text| Settings::decode(&text));
    let stats = offset.and_then(|offset| read_record(&mut flash, offset, STATS_SECTOR));
    let ap_name = custom_ap_name(&mut flash).unwrap_or_else(|| bounded(AP_SSID));
    match (offset, &stored) {
        (None, _) => warn!("settings: no nvs partition, nothing can be saved"),
        (Some(_), Some(_)) => info!("settings: loaded"),
        (Some(_), None) => info!("settings: none stored"),
    }
    let state = State {
        store: offset.map(|offset| Store { flash, offset }),
        stored: stored.is_some(),
        settings: stored.unwrap_or_default(),
        ap_name,
        stats,
    };
    STATE.lock(|cell| *cell.borrow_mut() = Some(state));
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.lock(|cell| f(cell.borrow_mut().as_mut().expect("settings::init first")))
}

pub fn get() -> Settings {
    with(|state| state.settings.clone())
}

pub fn timezone() -> i32 {
    with(|state| state.settings.timezone)
}

/// Whether flash held settings at boot, `nvMem.loadConfig`.
pub fn stored() -> bool {
    with(|state| state.stored)
}

/// The flash chip's size in bytes, for the portal's info page.
pub fn flash_size() -> usize {
    with(|state| state.store.as_ref().map_or(0, |store| store.flash.capacity()))
}

pub fn ap_name() -> String<32> {
    with(|state| state.ap_name.clone())
}

/// `nvMem.saveConfig`.
pub fn save(settings: &Settings) -> bool {
    let text = settings.encode();
    with(|state| {
        let Some(store) = state.store.as_mut() else {
            return false;
        };
        match write_record(store, SETTINGS_SECTOR, &text) {
            Ok(()) => {
                info!("settings: saved");
                state.settings = settings.clone();
                state.stored = true;
                true
            }
            Err(error) => {
                warn!("settings: save failed: {:?}", error);
                false
            }
        }
    })
}

/// `nvMem.deleteConfig` and `resetStat`: settings and statistics both go.
pub fn erase() -> bool {
    with(|state| {
        let Some(store) = state.store.as_mut() else {
            return false;
        };
        let end = store.offset + SECTORS * SECTOR;
        let erased = store.flash.erase(store.offset, end).is_ok();
        if erased {
            info!("settings: erased, with the statistics");
            state.stored = false;
        }
        erased
    })
}

/// The statistics record found at boot, once.
pub fn take_stats() -> Option<String<RECORD_MAX>> {
    with(|state| state.stats.take())
}

/// `saveStat`'s write.
pub fn save_stats(text: &str) -> bool {
    with(|state| {
        let Some(store) = state.store.as_mut() else {
            return false;
        };
        let written = write_record(store, STATS_SECTOR, text);
        if let Err(error) = &written {
            warn!("statistics: save failed: {:?}", error);
        }
        written.is_ok()
    })
}
