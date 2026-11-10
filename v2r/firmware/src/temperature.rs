//! The ESP32-S3's internal temperature sensor, which esp-hal does not drive on
//! this chip. Arduino's `temperatureRead` on the S3 goes through ESP-IDF's
//! `temperature_sensor` driver in its -10..80 °C range. This does the same with
//! the register writes of IDF's `temperature_sensor_ll.h` for the S3.

use core::ptr::{read_volatile, write_volatile};

const SENS_BASE: usize = 0x6000_8800;
const TSENS_CTRL: *mut u32 = (SENS_BASE + 0x50) as *mut u32;
const TSENS_CTRL2: *mut u32 = (SENS_BASE + 0x54) as *mut u32;
const PERI_CLK_GATE_CONF: *mut u32 = (SENS_BASE + 0x104) as *mut u32;

const TSENS_OUT: u32 = 0xff;
const TSENS_READY: u32 = 1 << 8;
const TSENS_POWER_UP: u32 = 1 << 22;
const TSENS_POWER_UP_FORCE: u32 = 1 << 23;
const TSENS_DUMP_OUT: u32 = 1 << 24;
const TSENS_CLK_SEL: u32 = 1 << 15;
const TSENS_CLK_EN: u32 = 1 << 29;

fn modify(register: *mut u32, set: u32, clear: u32) {
    unsafe { write_volatile(register, (read_volatile(register) & !clear) | set) }
}

pub fn enable() {
    modify(PERI_CLK_GATE_CONF, TSENS_CLK_EN, 0);
    modify(TSENS_CTRL2, TSENS_CLK_SEL, 0);
    modify(TSENS_CTRL, TSENS_POWER_UP_FORCE | TSENS_POWER_UP, 0);
}

/// Degrees Celsius, with IDF's conversion for the -10..80 °C range.
pub fn read() -> f32 {
    modify(TSENS_CTRL, TSENS_DUMP_OUT, 0);
    let mut spins = 0;
    while unsafe { read_volatile(TSENS_CTRL) } & TSENS_READY == 0 && spins < 10_000 {
        spins += 1;
    }
    let raw = unsafe { read_volatile(TSENS_CTRL) } & TSENS_OUT;
    modify(TSENS_CTRL, 0, TSENS_DUMP_OUT);
    0.4386 * raw as f32 - 20.52
}
