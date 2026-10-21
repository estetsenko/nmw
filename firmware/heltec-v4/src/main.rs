//! NMW firmware for the Heltec WiFi LoRa 32 V4: joins `#public`, shows
//! incoming text messages and the GPS fix on the OLED, and shares the
//! position on request.
//!
//! The button: a short press shows the next page or message, then the GPS
//! and node screens; a long press shares the position (GPS screen),
//! announces the node (node screen) or jumps to the newest message.
//!
//! Pins are from Meshtastic's `variants/esp32s3/heltec_v4/variant.h`.

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

extern crate alloc;

use alloc::format;
use embedded_hal_bus::spi::ExclusiveDevice;
use esp_backtrace as _;
use esp_bootloader_esp_idf::partitions::{self, DataPartitionSubType, PartitionType};
use esp_hal::clock::CpuClock;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::main;
use esp_hal::rng::{Trng, TrngSource};
use esp_hal::spi::Mode;
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::{Instant, Rate};
use esp_hal::uart::{Config as UartConfig, Uart};
use esp_storage::FlashStorage;
use log::{info, warn};
use nmw_core::{Identity, LoraProfile};
use nmw_device::app::{Device, LONG_PRESS, Press, hex};
use nmw_device::sx1262::{RadioConfig, RadioEvent, Sx1262};
use nmw_device::ui;
use ssd1306::mode::DisplayConfig;
use ssd1306::prelude::*;
use ssd1306::{I2CDisplayInterface, Ssd1306};

esp_bootloader_esp_idf::esp_app_desc!();

/// EU 868 sub-band P (869.4-869.65 MHz, 10% duty cycle, 500 mW ERP).
const FREQUENCY_HZ: u32 = 869_525_000;
/// At the SX1262 pin; the V4's front-end amplifier adds about 10 dB, which
/// keeps the board well under the band's 27 dBm limit.
const TX_POWER_DBM: i8 = 12;
/// Button contact bounce.
const DEBOUNCE: u64 = 30_000;
/// The identity seed record at the start of the `nvs` partition.
const SEED_MAGIC: &[u8; 4] = b"NMW1";

fn now() -> u64 {
    Instant::now().duration_since_epoch().as_micros()
}

#[main]
fn main() -> ! {
    esp_println::logger::init_logger_from_env();
    let p = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);
    esp_alloc::heap_allocator!(size: 64 * 1024);

    let out = OutputConfig::default();
    let delay = Delay::new();

    // Vext (active low) powers the OLED and the GPS.
    let _vext = Output::new(p.GPIO36, Level::Low, out);
    // The LoRa front end: LDO on, chip enabled, PA mode (V4.2), RX LNA (V4.3).
    let _pa_power = Output::new(p.GPIO7, Level::High, out);
    let _pa_csd = Output::new(p.GPIO2, Level::High, out);
    let _pa_cps = Output::new(p.GPIO46, Level::High, out);
    let _pa_ctx = Output::new(p.GPIO5, Level::Low, out);
    let mut led = Output::new(p.GPIO35, Level::Low, out);
    let button = Input::new(p.GPIO0, InputConfig::default().with_pull(Pull::Up));
    delay.delay_millis(50);

    // Identity: a seed kept in flash, drawn from the true RNG on first boot.
    let _trng_source = TrngSource::new(p.RNG, p.ADC1);
    let trng = Trng::try_new().expect("TRNG source is enabled");
    let mut flash = FlashStorage::new(p.FLASH);
    let seed = load_or_create_seed(&mut flash, &trng);
    let identity = Identity::from_seed(&seed);
    let address = identity.address();
    let name = format!("Heltec {}", &hex(&address.0)[..4]);
    info!("NMW node {name}, address {}", hex(&address.0));
    let mut device = Device::new(
        identity,
        &name,
        u64::from(trng.random()) << 32 | u64::from(trng.random()),
        now(),
    );

    // OLED: SSD1315 on I2C0, reset on GPIO21.
    let mut oled_reset = Output::new(p.GPIO21, Level::Low, out);
    delay.delay_millis(5);
    oled_reset.set_high();
    delay.delay_millis(5);
    let i2c = I2c::new(
        p.I2C0,
        I2cConfig::default().with_frequency(Rate::from_khz(400)),
    )
    .expect("I2C config")
    .with_sda(p.GPIO17)
    .with_scl(p.GPIO18);
    let mut display = Ssd1306::new(
        I2CDisplayInterface::new(i2c),
        DisplaySize128x64,
        DisplayRotation::Rotate0,
    )
    .into_buffered_graphics_mode();
    if display.init().is_err() {
        warn!("OLED did not answer");
    }

    // SX1262 on SPI2.
    let spi = Spi::new(
        p.SPI2,
        SpiConfig::default()
            .with_frequency(Rate::from_mhz(8))
            .with_mode(Mode::_0),
    )
    .expect("SPI config")
    .with_sck(p.GPIO9)
    .with_mosi(p.GPIO10)
    .with_miso(p.GPIO11);
    let cs = Output::new(p.GPIO8, Level::High, out);
    let spi = ExclusiveDevice::new_no_delay(spi, cs).expect("SPI device");
    let busy = Input::new(p.GPIO13, InputConfig::default());
    let reset = Output::new(p.GPIO12, Level::High, out);
    let mut radio = Sx1262::new(spi, busy, reset, delay);
    let radio_ok = match radio.init(&RadioConfig {
        frequency_hz: FREQUENCY_HZ,
        profile: LoraProfile::MEDIUM,
        tx_power_dbm: TX_POWER_DBM,
        tcxo_voltage: Some(0x02),
        dio2_rf_switch: true,
    }) {
        Ok(()) => true,
        Err(e) => {
            warn!("radio init failed: {e:?}");
            false
        }
    };

    // L76K GPS on UART1: enable (active low) and wake it.
    let _gps_en = Output::new(p.GPIO34, Level::Low, out);
    let _gps_standby = Output::new(p.GPIO40, Level::High, out);
    let _gps_reset = Output::new(p.GPIO42, Level::High, out);
    let mut gps = Uart::new(p.UART1, UartConfig::default().with_baudrate(9600))
        .expect("UART config")
        .with_rx(p.GPIO38)
        .with_tx(p.GPIO39);

    let mut rx = [0u8; 255];
    let mut nmea = [0u8; 64];
    let mut screen_on = true;
    let mut pressed_at: Option<u64> = None;
    let mut last_edge = 0;
    let mut led_off_at = 0;

    loop {
        let t = now();

        if radio_ok {
            match radio.poll(&mut rx) {
                Ok(Some(RadioEvent::Received {
                    len,
                    rssi_dbm,
                    snr_db,
                })) => {
                    info!("rx {len} bytes, {rssi_dbm} dBm, SNR {snr_db}");
                    device.on_frame(&rx[..len], snr_db, t);
                    led.set_high();
                    led_off_at = t + 50_000;
                }
                Ok(Some(RadioEvent::TxDone)) => {}
                Ok(None) => {}
                Err(e) => warn!("radio: {e:?}"),
            }
        }

        device.tick(t);

        if radio_ok
            && !radio.is_transmitting()
            && let Some(frame) = device.poll_transmit(t)
            && let Err(e) = radio.transmit(&frame)
        {
            warn!("transmit: {e:?}");
        }

        if let Ok(n) = gps.read_buffered(&mut nmea)
            && n > 0
        {
            device.on_gps(&nmea[..n]);
        }

        // The button reads low while held.
        let down = button.is_low();
        if down != pressed_at.is_some() && t - last_edge > DEBOUNCE {
            last_edge = t;
            match pressed_at.take() {
                None => pressed_at = Some(t),
                Some(start) => {
                    let press = if t - start >= LONG_PRESS {
                        Press::Long
                    } else {
                        Press::Short
                    };
                    device.on_button(press, t);
                }
            }
        }

        if led_off_at != 0 && t >= led_off_at {
            led.set_low();
            led_off_at = 0;
        }

        if device.screen_on() != screen_on {
            screen_on = device.screen_on();
            let _ = display.set_display_on(screen_on);
        }
        if device.take_redraw() && screen_on {
            display.clear_buffer();
            let _ = ui::draw(&device, &mut display, t);
            if display.flush().is_err() {
                warn!("OLED flush failed");
            }
        }
    }
}

/// Read the 32-byte identity seed from the start of the `nvs` partition, or
/// write a fresh one there. Nothing else on this firmware uses NVS.
fn load_or_create_seed(flash: &mut FlashStorage<'_>, trng: &Trng) -> [u8; 32] {
    let mut table = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
    let nvs = partitions::read_partition_table(flash, &mut table)
        .ok()
        .and_then(|pt| {
            pt.find_partition(PartitionType::Data(DataPartitionSubType::Nvs))
                .ok()
                .flatten()
        });
    let Some(nvs) = nvs else {
        warn!("no nvs partition: the identity changes on every boot");
        let mut seed = [0u8; 32];
        trng.read(&mut seed);
        return seed;
    };
    let mut region = nvs.as_flash_region(flash);
    let mut record = [0u8; 36];
    if region.read(0, &mut record).is_ok() && &record[..4] == SEED_MAGIC {
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&record[4..]);
        return seed;
    }
    record[..4].copy_from_slice(SEED_MAGIC);
    trng.read(&mut record[4..]);
    match region.write(0, &record) {
        Ok(()) => info!("created a new identity"),
        Err(e) => warn!("could not save the identity: {e:?}"),
    }
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&record[4..]);
    seed
}
