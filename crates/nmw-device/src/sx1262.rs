//! Minimal blocking driver for the Semtech SX1262 LoRa transceiver: LoRa
//! packets only, explicit header, CRC on. Command codes and register
//! addresses are from the SX1261/2 datasheet (rev 2.1), sections 13 and 9.
//!
//! The radio sits in continuous receive. [`Sx1262::transmit`] starts a
//! transmission and returns at once; [`Sx1262::poll`] reports when it is done
//! (and the caller goes back to receive) or when a packet came in.

use embedded_hal::delay::DelayNs;
use embedded_hal::digital::{InputPin, OutputPin};
use embedded_hal::spi::{Operation, SpiDevice};
use nmw_core::LoraProfile;

const SET_SLEEP: u8 = 0x84;
const SET_STANDBY: u8 = 0x80;
const SET_TX: u8 = 0x83;
const SET_RX: u8 = 0x82;
const SET_PACKET_TYPE: u8 = 0x8A;
const SET_RF_FREQUENCY: u8 = 0x86;
const SET_TX_PARAMS: u8 = 0x8E;
const SET_PA_CONFIG: u8 = 0x95;
const SET_MODULATION_PARAMS: u8 = 0x8B;
const SET_PACKET_PARAMS: u8 = 0x8C;
const SET_BUFFER_BASE_ADDRESS: u8 = 0x8F;
const SET_DIO_IRQ_PARAMS: u8 = 0x08;
const SET_DIO2_AS_RF_SWITCH: u8 = 0x9D;
const SET_DIO3_AS_TCXO: u8 = 0x97;
const SET_REGULATOR_MODE: u8 = 0x96;
const CALIBRATE: u8 = 0x89;
const CALIBRATE_IMAGE: u8 = 0x98;
const WRITE_REGISTER: u8 = 0x0D;
const WRITE_BUFFER: u8 = 0x0E;
const READ_BUFFER: u8 = 0x1E;
const GET_IRQ_STATUS: u8 = 0x12;
const CLEAR_IRQ_STATUS: u8 = 0x02;
const GET_RX_BUFFER_STATUS: u8 = 0x13;
const GET_PACKET_STATUS: u8 = 0x14;

const REG_SYNC_WORD: u16 = 0x0740;
/// Over-current protection; 0x38 = 140 mA, recommended for +22 dBm.
const REG_OCP: u16 = 0x08E7;
/// Datasheet 15.4: set bit 1 after reset for better antenna mismatch handling.
const REG_TX_CLAMP: u16 = 0x08D8;

pub const IRQ_TX_DONE: u16 = 1 << 0;
pub const IRQ_RX_DONE: u16 = 1 << 1;
pub const IRQ_HEADER_ERR: u16 = 1 << 5;
pub const IRQ_CRC_ERR: u16 = 1 << 6;
pub const IRQ_TIMEOUT: u16 = 1 << 9;
const IRQ_USED: u16 = IRQ_TX_DONE | IRQ_RX_DONE | IRQ_HEADER_ERR | IRQ_CRC_ERR | IRQ_TIMEOUT;

/// LoRa sync word "0x12" (the SX126x "private network" value), stored in the
/// register as 0x1424.
pub const SYNC_WORD: u16 = 0x1424;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RadioConfig {
    pub frequency_hz: u32,
    pub profile: LoraProfile,
    /// Power at the SX1262 output, -9..=22 dBm. Boards with an external PA
    /// add their gain on top.
    pub tx_power_dbm: i8,
    /// The board drives a TCXO from DIO3 at this voltage code (0x02 = 1.8 V).
    pub tcxo_voltage: Option<u8>,
    /// DIO2 switches the antenna between transmit and receive.
    pub dio2_rf_switch: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error<E> {
    Spi(E),
    /// BUSY stayed high: the chip is not responding.
    Busy,
    /// A pin could not be read or set.
    Pin,
    /// The frame is longer than 255 bytes.
    TooLong,
}

/// What [`Sx1262::poll`] saw.
#[derive(Debug, PartialEq)]
pub enum RadioEvent {
    /// A transmission finished; the radio is back in receive.
    TxDone,
    /// A packet with a good CRC: its length is in `len`, read it with the
    /// returned buffer slice.
    Received {
        len: usize,
        rssi_dbm: i16,
        snr_db: f32,
    },
}

pub struct Sx1262<SPI, BUSY, RST, D> {
    spi: SPI,
    busy: BUSY,
    reset: RST,
    delay: D,
    transmitting: bool,
    profile: LoraProfile,
}

impl<SPI, BUSY, RST, D, E> Sx1262<SPI, BUSY, RST, D>
where
    SPI: SpiDevice<Error = E>,
    BUSY: InputPin,
    RST: OutputPin,
    D: DelayNs,
{
    pub fn new(spi: SPI, busy: BUSY, reset: RST, delay: D) -> Self {
        Sx1262 {
            spi,
            busy,
            reset,
            delay,
            transmitting: false,
            profile: LoraProfile::MEDIUM,
        }
    }

    /// Reset the chip, configure LoRa and start receiving.
    pub fn init(&mut self, cfg: &RadioConfig) -> Result<(), Error<E>> {
        self.reset.set_low().map_err(|_| Error::Pin)?;
        self.delay.delay_ms(2);
        self.reset.set_high().map_err(|_| Error::Pin)?;
        self.delay.delay_ms(10);
        self.cmd(SET_STANDBY, &[0x00])?; // RC oscillator
        if let Some(v) = cfg.tcxo_voltage {
            // Wait 5 ms for the TCXO to settle (units of 15.625 us).
            self.cmd(SET_DIO3_AS_TCXO, &[v, 0x00, 0x01, 0x40])?;
        }
        self.cmd(CALIBRATE, &[0x7F])?;
        self.delay.delay_ms(5);
        self.cmd(SET_REGULATOR_MODE, &[0x01])?; // DC-DC
        if cfg.dio2_rf_switch {
            self.cmd(SET_DIO2_AS_RF_SWITCH, &[0x01])?;
        }
        self.cmd(SET_PACKET_TYPE, &[0x01])?; // LoRa
        let [lo, hi] = image_cal_band(cfg.frequency_hz);
        self.cmd(CALIBRATE_IMAGE, &[lo, hi])?;
        self.cmd(SET_RF_FREQUENCY, &freq_word(cfg.frequency_hz).to_be_bytes())?;
        // +22 dBm capable PA settings (datasheet table 13-21).
        self.cmd(SET_PA_CONFIG, &[0x04, 0x07, 0x00, 0x01])?;
        self.write_register(REG_OCP, &[0x38])?;
        self.write_register(REG_TX_CLAMP, &[0xDE])?;
        let power = cfg.tx_power_dbm.clamp(-9, 22);
        self.cmd(SET_TX_PARAMS, &[power as u8, 0x04])?; // 200 us ramp
        self.cmd(SET_BUFFER_BASE_ADDRESS, &[0x00, 0x00])?;
        self.cmd(SET_MODULATION_PARAMS, &modulation_params(&cfg.profile))?;
        self.profile = cfg.profile;
        self.set_packet_params(255)?;
        self.write_register(REG_SYNC_WORD, &SYNC_WORD.to_be_bytes())?;
        let mask = IRQ_USED.to_be_bytes();
        self.cmd(
            SET_DIO_IRQ_PARAMS,
            &[mask[0], mask[1], mask[0], mask[1], 0, 0, 0, 0],
        )?;
        self.start_rx()
    }

    /// Continuous receive.
    pub fn start_rx(&mut self) -> Result<(), Error<E>> {
        self.transmitting = false;
        self.set_packet_params(255)?;
        self.cmd(SET_RX, &[0xFF, 0xFF, 0xFF])
    }

    /// Whether a transmission is under way.
    pub fn is_transmitting(&self) -> bool {
        self.transmitting
    }

    /// Start sending `frame`. [`Sx1262::poll`] reports [`RadioEvent::TxDone`].
    pub fn transmit(&mut self, frame: &[u8]) -> Result<(), Error<E>> {
        if frame.len() > 255 {
            return Err(Error::TooLong);
        }
        self.cmd(SET_STANDBY, &[0x00])?;
        self.set_packet_params(frame.len() as u8)?;
        self.wait_busy()?;
        self.spi
            .transaction(&mut [
                Operation::Write(&[WRITE_BUFFER, 0x00]),
                Operation::Write(frame),
            ])
            .map_err(Error::Spi)?;
        self.cmd(SET_TX, &[0x00, 0x00, 0x00])?; // no timeout
        self.transmitting = true;
        Ok(())
    }

    /// Check for a finished transmission or a received packet. A received
    /// packet is copied into `buf`.
    pub fn poll(&mut self, buf: &mut [u8; 255]) -> Result<Option<RadioEvent>, Error<E>> {
        let irq = self.irq_status()?;
        if irq == 0 {
            return Ok(None);
        }
        self.cmd(CLEAR_IRQ_STATUS, &irq.to_be_bytes())?;
        if irq & IRQ_TX_DONE != 0 && self.transmitting {
            self.start_rx()?;
            return Ok(Some(RadioEvent::TxDone));
        }
        if irq & IRQ_RX_DONE == 0 || irq & (IRQ_CRC_ERR | IRQ_HEADER_ERR) != 0 {
            return Ok(None);
        }
        let mut st = [GET_RX_BUFFER_STATUS, 0, 0, 0];
        self.read_cmd(&mut st)?;
        let (len, start) = (st[2] as usize, st[3]);
        self.wait_busy()?;
        self.spi
            .transaction(&mut [
                Operation::Write(&[READ_BUFFER, start, 0x00]),
                Operation::Read(&mut buf[..len]),
            ])
            .map_err(Error::Spi)?;
        let mut ps = [GET_PACKET_STATUS, 0, 0, 0, 0];
        self.read_cmd(&mut ps)?;
        Ok(Some(RadioEvent::Received {
            len,
            rssi_dbm: -(ps[2] as i16) / 2,
            snr_db: (ps[3] as i8) as f32 / 4.0,
        }))
    }

    /// Lowest-power sleep; [`Sx1262::init`] wakes it again.
    pub fn sleep(&mut self) -> Result<(), Error<E>> {
        self.transmitting = false;
        self.cmd(SET_SLEEP, &[0x00])
    }

    fn set_packet_params(&mut self, len: u8) -> Result<(), Error<E>> {
        let p = self.profile.preamble_symbols.to_be_bytes();
        // Explicit header, CRC on, standard IQ.
        self.cmd(SET_PACKET_PARAMS, &[p[0], p[1], 0x00, len, 0x01, 0x00])
    }

    fn irq_status(&mut self) -> Result<u16, Error<E>> {
        let mut b = [GET_IRQ_STATUS, 0, 0, 0];
        self.read_cmd(&mut b)?;
        Ok(u16::from_be_bytes([b[2], b[3]]))
    }

    fn write_register(&mut self, addr: u16, data: &[u8]) -> Result<(), Error<E>> {
        let a = addr.to_be_bytes();
        self.wait_busy()?;
        self.spi
            .transaction(&mut [
                Operation::Write(&[WRITE_REGISTER, a[0], a[1]]),
                Operation::Write(data),
            ])
            .map_err(Error::Spi)
    }

    fn cmd(&mut self, op: u8, args: &[u8]) -> Result<(), Error<E>> {
        self.wait_busy()?;
        self.spi
            .transaction(&mut [Operation::Write(&[op]), Operation::Write(args)])
            .map_err(Error::Spi)
    }

    /// A command whose reply comes back in the same bytes: `buf[0]` is the
    /// opcode, the rest is filled in (status byte first).
    fn read_cmd(&mut self, buf: &mut [u8]) -> Result<(), Error<E>> {
        self.wait_busy()?;
        self.spi.transfer_in_place(buf).map_err(Error::Spi)
    }

    fn wait_busy(&mut self) -> Result<(), Error<E>> {
        for _ in 0..10_000 {
            if !self.busy.is_high().map_err(|_| Error::Pin)? {
                return Ok(());
            }
            self.delay.delay_us(10);
        }
        Err(Error::Busy)
    }
}

/// RF frequency register value: f * 2^25 / 32 MHz, rounded down.
pub fn freq_word(hz: u32) -> u32 {
    (((hz as u64) << 25) / 32_000_000) as u32
}

/// Image calibration band (datasheet table 9-2).
fn image_cal_band(hz: u32) -> [u8; 2] {
    match hz {
        f if f > 900_000_000 => [0xE1, 0xE9],
        f if f > 850_000_000 => [0xD7, 0xDB],
        f if f > 770_000_000 => [0xC1, 0xC5],
        f if f > 460_000_000 => [0x75, 0x81],
        _ => [0x6B, 0x6F],
    }
}

fn modulation_params(p: &LoraProfile) -> [u8; 4] {
    let bw = match p.bandwidth_hz {
        7_800 => 0x00,
        15_600 => 0x01,
        31_250 => 0x02,
        62_500 => 0x03,
        125_000 => 0x04,
        250_000 => 0x05,
        500_000 => 0x06,
        _ => 0x05,
    };
    // Low data rate optimisation is required when a symbol lasts over 16 ms.
    let ldro = (p.symbol_us() > 16_000) as u8;
    [p.spreading_factor, bw, p.coding_rate, ldro]
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use core::convert::Infallible;

    /// Records every SPI transaction as one byte string, and answers
    /// transfers and reads from a script.
    #[derive(Default)]
    struct FakeSpi {
        log: Vec<Vec<u8>>,
        replies: Vec<Vec<u8>>,
    }

    impl embedded_hal::spi::ErrorType for FakeSpi {
        type Error = Infallible;
    }

    impl SpiDevice for FakeSpi {
        fn transaction(&mut self, ops: &mut [Operation<'_, u8>]) -> Result<(), Infallible> {
            let mut out = Vec::new();
            for op in ops {
                match op {
                    Operation::Write(w) => out.extend_from_slice(w),
                    Operation::TransferInPlace(b) => {
                        out.extend_from_slice(b);
                        let r = self.replies.remove(0);
                        b.copy_from_slice(&r);
                    }
                    Operation::Read(b) => {
                        let r = self.replies.remove(0);
                        b.copy_from_slice(&r);
                    }
                    _ => unimplemented!(),
                }
            }
            self.log.push(out);
            Ok(())
        }
    }

    struct Pin;
    impl embedded_hal::digital::ErrorType for Pin {
        type Error = Infallible;
    }
    impl InputPin for Pin {
        fn is_high(&mut self) -> Result<bool, Infallible> {
            Ok(false)
        }
        fn is_low(&mut self) -> Result<bool, Infallible> {
            Ok(true)
        }
    }
    impl OutputPin for Pin {
        fn set_low(&mut self) -> Result<(), Infallible> {
            Ok(())
        }
        fn set_high(&mut self) -> Result<(), Infallible> {
            Ok(())
        }
    }
    struct NoDelay;
    impl DelayNs for NoDelay {
        fn delay_ns(&mut self, _: u32) {}
    }

    fn radio() -> Sx1262<FakeSpi, Pin, Pin, NoDelay> {
        Sx1262::new(FakeSpi::default(), Pin, Pin, NoDelay)
    }

    const CFG: RadioConfig = RadioConfig {
        frequency_hz: 869_525_000,
        profile: LoraProfile::MEDIUM,
        tx_power_dbm: 12,
        tcxo_voltage: Some(0x02),
        dio2_rf_switch: true,
    };

    #[test]
    fn frequency_word_matches_the_datasheet_formula() {
        // 869.525 MHz * 2^25 / 32 MHz = 911_763_046.4
        assert_eq!(freq_word(869_525_000), 911_763_046);
        assert_eq!(freq_word(915_000_000), 959_447_040);
    }

    #[test]
    fn init_configures_lora_and_ends_in_receive() {
        let mut r = radio();
        r.init(&CFG).unwrap();
        let log = &r.spi.log;
        let has = |cmd: &[u8]| log.iter().any(|t| t.as_slice() == cmd);
        assert!(has(&[SET_PACKET_TYPE, 0x01]));
        assert!(has(&[SET_DIO3_AS_TCXO, 0x02, 0x00, 0x01, 0x40]));
        assert!(has(&[SET_DIO2_AS_RF_SWITCH, 0x01]));
        assert!(has(&[CALIBRATE_IMAGE, 0xD7, 0xDB]));
        let f = freq_word(869_525_000).to_be_bytes();
        assert!(has(&[SET_RF_FREQUENCY, f[0], f[1], f[2], f[3]]));
        assert!(has(&[SET_MODULATION_PARAMS, 9, 0x05, 1, 0]));
        assert!(has(&[SET_TX_PARAMS, 12, 0x04]));
        assert!(has(&[WRITE_REGISTER, 0x07, 0x40, 0x14, 0x24]));
        assert_eq!(log.last().unwrap(), &[SET_RX, 0xFF, 0xFF, 0xFF]);
        // The long-range profile needs low data rate optimisation.
        assert_eq!(modulation_params(&LoraProfile::LONG), [11, 0x05, 1, 0]);
        let slow = LoraProfile {
            bandwidth_hz: 125_000,
            spreading_factor: 12,
            ..LoraProfile::LONG
        };
        assert_eq!(modulation_params(&slow)[3], 1);
    }

    #[test]
    fn transmit_then_tx_done_returns_to_receive() {
        let mut r = radio();
        r.transmit(&[1, 2, 3]).unwrap();
        assert!(r.is_transmitting());
        assert!(r
            .spi
            .log
            .iter()
            .any(|t| t.as_slice() == [WRITE_BUFFER, 0, 1, 2, 3]));
        assert!(r
            .spi
            .log
            .iter()
            .any(|t| t.as_slice() == [SET_PACKET_PARAMS, 0, 16, 0, 3, 1, 0]));
        r.spi.replies.push(alloc::vec![0, 0, 0, IRQ_TX_DONE as u8]);
        let mut buf = [0u8; 255];
        assert_eq!(r.poll(&mut buf).unwrap(), Some(RadioEvent::TxDone));
        assert!(!r.is_transmitting());
        assert_eq!(r.spi.log.last().unwrap(), &[SET_RX, 0xFF, 0xFF, 0xFF]);
        assert!(r.transmit(&[0; 256]).is_err());
    }

    #[test]
    fn received_packet_is_read_with_signal_quality() {
        let mut r = radio();
        r.spi.replies.push(alloc::vec![0, 0, 0, IRQ_RX_DONE as u8]); // irq
        r.spi.replies.push(alloc::vec![0, 0, 3, 0x10]); // 3 bytes at 0x10
        r.spi.replies.push(alloc::vec![7, 8, 9]); // payload
        r.spi.replies.push(alloc::vec![0, 0, 180, (-22i8) as u8, 0]); // -90 dBm, -5.5 dB
        let mut buf = [0u8; 255];
        let ev = r.poll(&mut buf).unwrap();
        assert_eq!(
            ev,
            Some(RadioEvent::Received {
                len: 3,
                rssi_dbm: -90,
                snr_db: -5.5
            })
        );
        assert_eq!(&buf[..3], &[7, 8, 9]);
        assert!(r
            .spi
            .log
            .iter()
            .any(|t| t.as_slice() == [READ_BUFFER, 0x10, 0]));

        // A CRC error is cleared and dropped.
        r.spi
            .replies
            .push(alloc::vec![0, 0, 0, (IRQ_RX_DONE | IRQ_CRC_ERR) as u8]);
        assert_eq!(r.poll(&mut buf).unwrap(), None);
        // Nothing pending.
        r.spi.replies.push(alloc::vec![0, 0, 0, 0]);
        assert_eq!(r.poll(&mut buf).unwrap(), None);
    }
}
