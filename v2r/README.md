# NerdMiner v2R

The Rust rewrite of NerdMiner v2 for the ESP32-S3, starting with the board this
fork is tested on: the LilyGo T-Display S3 (`NerdminerV2` in `platformio.ini`).
The C++ firmware stays in the repository as the reference until the Rust one
matches it.

## Building

Install the Xtensa Rust toolchain once:

```powershell
cargo install espup espflash --locked
espup install --targets esp32s3
```

Optionally, copy `v2r/.env.example` to `v2r/.env` and fill it in: its values
are what the setup portal's form starts with, so that saving it is one tap.
They are baked into the firmware, so do not share a binary built with them.
Then, in each new PowerShell window, put the Xtensa linker on `PATH` and
build:

```powershell
. ./v2r/env.ps1
cd v2r
cargo build --release -p nerdminer-v2r
espflash flash --chip esp32s3 --monitor target/xtensa-esp32s3-none-elf/release/nerdminer-v2r
```

`-p v2r-bench` builds the kernel bench instead.

## `firmware`: the miner

Core 0 drives the SHA accelerator in a bare loop: no scheduler, no interrupt,
nothing else. Core 1 runs esp-rtos, with Wi-Fi, the network stack and the
Stratum client in an embassy executor, and a software miner at a lower priority
that takes the time they leave. The accelerator mines the lower half of the
nonces in the first header version. The software takes the upper half in all
four rolled versions (BIP 310). Every hit is hashed again in software before it
is submitted, and the log counts any that do not match.

Measured on 10 Nov. 2026 on pool.solomining.de, screen on: **367.5 kH/s**
(331 from the accelerator, 36.4 in software), every share accepted. NerdMiner
v1.8.3 makes about 253 kH/s on the same board.

How it gets there:

- The accelerator loop writes the next message words while a block is being
  hashed, and never rewrites the words that stay zero (see `bench` below).
- It waits 150 cycles after starting the first block, then reads the busy
  flag once; after the second block it does not wait at all, since filling the
  next first block takes longer. Timings were swept on the board with the
  `tune` feature (`cargo build --release -p nerdminer-v2r --features tune`).
  Skipping the first busy read is faster, but then the block's length depends
  on what core 1 does on the bus, and hashes come out wrong.
- After each chunk of 4096 nonces, one nonce is hashed again in software. A
  mismatch counts as a bad check in the log, and switches the loop for good to
  waiting on the busy flag after both blocks.
- The software loop (`sw.rs`) is PicoMiner's, with each 32-bit rotation done
  by the LX7's funnel shift, `ssai` and `src`, in two instructions instead of
  three. It is checked against PicoMiner's at the first job, and falls back
  to it on a mismatch.

The T-Display S3 screens match the C++ firmware's: the loading screen, then
the miner, clock, global hashrate and BTC price screens, with the same
backgrounds, fonts and places. `v2r/tools/assets.py` converts the images and
fonts from `src/media` and TFT_eSPI, and `v2r/tools/preview.py` draws every
screen on the PC to check them before flashing. The text is laid out the way
OpenFontRender and TFT_eSPI do it. Only what changes is redrawn. The network
figures come from the same APIs, over HTTPS without certificate checks, like
the C++ firmware. The buttons work the same way: on the BOOT side, a click
turns the screen off or on and a double click flips it; on the other side, a
click shows the next screen.

The setup portal is WiFiManager's, as the C++ firmware runs it. With no
settings stored, with the second button held at boot, or when the stored
network does not answer within 40 s, the board shows the setup screen and
opens the `NerdMinerAP` access point, password `MineYourCoins`. Joining it
opens the portal at `http://192.168.4.1` by itself: the board gives out the
addresses and answers every DNS name with its own. The pages are
WiFiManager v2.0.17's, copied by `v2r/tools/wm_strings.py`, with the Wi-Fi
scan and NerdMiner's fields: pool, port, pool password, BTC address (with
`.worker` if you like), time zone and the statistics option. Saving writes
the settings to the first sector of the `nvs` partition and restarts. With no
web request for 180 s, the board restarts too. A long press on the second
button erases the settings and restarts into the portal.

Not ported yet: WiFiManager's firmware update page, which needs a partition
table with two app slots; the statistics kept in flash; and every board but
the T-Display S3.

## `bench`: the SHA-256 kernels

`bench` checks each mining kernel against the software reference from
[PicoMiner](https://github.com/Mouflon23/PicoMiner)'s core, on 2000 nonces, and
then times it. Measured on 9 Nov. 2026 on the T-Display S3 at 240 MHz, with
neither Wi-Fi nor screen running:

| Kernel | What it does | kH/s |
| --- | --- | --- |
| hw upstream | NerdMiner v1.8.3's loop: every message word of both blocks, every nonce | 231 |
| hw minimal | never rewrites message words 9..=14, zero in both blocks | 280 |
| hw pipe2 | fills the next nonce's first block while the accelerator hashes the second | 311 |
| hw pipe12 | pipe2, plus the second block's padding written during the first | 324 |
| hw fixed | pipe12 waiting a calibrated number of cycles instead of polling `BUSY` | 343 |
| sw rust ×1 | PicoMiner's scan, compiled for the Xtensa LX7 | 28 |
| sw rust ×4 | the same with four header versions per nonce (BIP 310) | 34 |
| **dual core** | **hw pipe12 on core 0, sw ×4 on core 1** | **358** |

For comparison, the stock C++ firmware shows about 253 kH/s on the same board, with Wi-Fi and the screen running.

One nonce of `hw minimal` takes about 850 cycles, and only 340 of them are
the accelerator hashing. The rest go to peripheral bus accesses: about
12 cycles per register write and 20 per read. Every gain above comes from hiding those
accesses behind the hashing. The accelerator takes in its message words as soon as
a block starts, so the next words can be written while it works.

`hw fixed` needs the delays measured for this clock. The firmware will wait
most of that delay and then poll `BUSY` once, which keeps it right if the clock
ever changes.
