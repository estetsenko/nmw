# NMW firmware for the Heltec V4

Standalone firmware for the Heltec WiFi LoRa 32 V4 (ESP32-S3, SX1262, L76K
GPS, 128x64 OLED). It joins **#public** on 869.525 MHz with the `medium`
profile (SF9, 250 kHz), shows incoming messages and the GPS fix on the
screen, and can share its position. It has not been tested on hardware yet.

The board logic (radio driver, NMEA reader, inbox and screens) lives in
[`crates/nmw-device`](../../crates/nmw-device) and is unit-tested on the host;
this crate only wires it to the pins.

## Using it

One button (PRG):

| Screen | Short press | Long press (hold ~1 s) |
|---|---|---|
| Message | next page, then the next older message | jump to the newest message |
| GPS | node screen | share your position in #public |
| Node | newest message | announce the node now |

Short presses go newest message → older ones → GPS → Node → newest again.
The header shows the channel or sender, how long ago, and `+n` for unread
messages. The screen turns off after a minute; any press or a new message
turns it back on. The LED blinks on every received packet.

Messages from #public and direct messages are shown; positions others share
show their coordinates and, once this board has a fix, the distance and
direction ("1.5 km NE of you"). Files show as a name and size only.

The node's identity (and so its address) is created on first boot and kept
in the `nvs` flash partition; its name is `Heltec` plus the first four hex
digits of the address.

## Flashing a CI build

The newest `nmw-heltec-v4.bin` from main is on the
[Latest build](https://github.com/estetsenko/nmw/releases/tag/latest-build)
release; every CI run (PRs too) also uploads it as the
`nmw-heltec-v4-firmware` artifact. It is a merged image for offset 0x0.
Flash it with
[espflash](https://github.com/esp-rs/espflash) or esptool:

```sh
espflash write-bin 0x0 nmw-heltec-v4.bin
# or
esptool.py --chip esp32s3 write_flash 0x0 nmw-heltec-v4.bin
```

If the board does not show up, hold PRG while plugging in USB.

## Building

Install Espressif's Rust toolchain and espflash once:

```sh
cargo install espup espflash
espup install
. ~/export-esp.sh
```

Then, from this directory:

```sh
cargo build --release
cargo run --release   # flash and open the serial log
```

`ESP_LOG=debug` in `.cargo/esp-config.toml` makes the log chattier.

## Pins

From Meshtastic's `variants/esp32s3/heltec_v4/variant.h`:

| What | Pins |
|---|---|
| SX1262 | NSS 8, SCK 9, MOSI 10, MISO 11, RST 12, BUSY 13, DIO1 14; DIO2 RF switch, DIO3 TCXO 1.8 V |
| LoRa front end | power 7, CSD 2 (on), CPS 46 (PA mode), CTX 5 (RX LNA) |
| OLED (SSD1315, 0x3C) | SDA 17, SCL 18, RST 21 |
| GPS (L76K, 9600 baud) | RX 38, TX 39, EN 34 (active low), STANDBY 40, RESET 42 |
| Other | Vext 36 (active low), button 0, LED 35 |

Transmit power is 12 dBm at the SX1262, about 22 dBm after the front-end
amplifier, under the 27 dBm ERP allowed at 869.4–869.65 MHz.
