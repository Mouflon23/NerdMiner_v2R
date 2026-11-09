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

Copy `v2r/.env.example` to `v2r/.env` and fill in the Wi-Fi network and your
BTC address. They are baked into the firmware, so do not share a binary built
with them. Then, in each new PowerShell window, put the Xtensa linker on `PATH`
and build:

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

Measured on 9 Nov. 2026 on pool.solomining.de, with Wi-Fi at −83 dBm:
**353 kH/s** (321 from the accelerator, 32 in software), every share accepted,
no bad hit. NerdMiner v1.8.3 makes about 253 kH/s on the same board.

Not ported yet: the screen, the setup portal, the buttons and the statistics
kept in flash.

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
