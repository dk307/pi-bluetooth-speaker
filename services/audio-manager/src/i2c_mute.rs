//! Serial mute control for the Merus MA12070P over I2C.
//!
//! Datasheet reference:
//! <https://www.infineon.com/assets/row/public/documents/24/49/infineon-ma12070p-datasheet-en.pdf>
//! (V1.1, 2022-02-09), §8.5 register table: register `0x36` bit 7
//! (`audio_proc_mute`) — 1 = mute, 0 = play. This is the only software mute
//! that persists while the kernel driver runs: the board's `/MUTE` GPIO line
//! (MA12070P pin 34, BCM GPIO15 here) is owned by the `ma120x0p` driver and
//! re-asserted on every PCM stream event, and the ALSA control map exposes no
//! mute switch.
//!
//! Live-verified on the Pi (2026-11): the device answers on I2C address
//! `0x20` (`0x23` NACKs all writes), a read-modify-write of `0x36` bit 7
//! mutes/restores playback mid-stream, and the setting persists because the
//! kernel driver never rewrites `0x36`.
//!
//! The daemon applies mute as **belt-and-braces**: the serial bit (instant,
//! authoritative) *and* the ALSA raw-0 level write (protects against a power
//! cycle, which resets `0x36` to the unmuted default `0x41`).

use rppal::i2c::I2c;

use crate::error::{AmpError, AmpResult};

/// I2C bus number for the amp (`/dev/i2c-1` on the Pi).
pub const MUTE_I2C_BUS: u8 = 1;
/// Live-verified I2C address of the MA12070P control interface.
pub const MA12070P_I2C_ADDR: u16 = 0x20;
/// Audio-processor control register holding the mute-mux bit.
const AUDIO_PROC_REG: u8 = 0x36;
/// `audio_proc_mute` bit position in `0x36` (1 = mute, 0 = play).
const AUDIO_PROC_MUTE_BIT: u8 = 7;

/// Returns `reg` with the mute bit set or cleared (read-modify-write step).
#[must_use]
pub fn apply_mute_bit(reg: u8, muted: bool) -> u8 {
    if muted {
        reg | (1 << AUDIO_PROC_MUTE_BIT)
    } else {
        reg & !(1 << AUDIO_PROC_MUTE_BIT)
    }
}

/// Extracts the mute state from a register read (pure, host-testable).
#[must_use]
pub fn is_muted_bit(reg: u8) -> bool {
    reg & (1 << AUDIO_PROC_MUTE_BIT) != 0
}

/// Opens the I2C bus and selects the amp slave address.
///
/// # Errors
/// Returns [`AmpError::I2c`] if the bus cannot be opened or the slave
/// address cannot be set.
pub fn open_amp_i2c() -> AmpResult<I2c> {
    let mut i2c = I2c::with_bus(MUTE_I2C_BUS).map_err(AmpError::I2c)?;
    i2c.set_slave_address(MA12070P_I2C_ADDR)
        .map_err(AmpError::I2c)?;
    Ok(i2c)
}

/// Reads register `0x36` (blocking).
///
/// # Errors
/// Returns [`AmpError::I2c`] on bus faults.
pub fn read_audio_proc_reg_blocking(i2c: &mut I2c) -> AmpResult<u8> {
    i2c.set_slave_address(MA12070P_I2C_ADDR)
        .map_err(AmpError::I2c)?;
    let mut buf = [0u8; 1];
    i2c.read(&mut buf).map_err(AmpError::I2c)?;
    Ok(buf[0])
}

/// Read-modify-write of register `0x36` to apply or clear the serial mute
/// (blocking). Verifies the write by re-reading the register.
///
/// # Errors
/// Returns [`AmpError::I2c`] on bus faults or a verification mismatch.
pub fn set_serial_mute_blocking(i2c: &mut I2c, muted: bool) -> AmpResult<()> {
    let reg = read_audio_proc_reg_blocking(i2c)?;
    let desired = apply_mute_bit(reg, muted);
    i2c.set_slave_address(MA12070P_I2C_ADDR)
        .map_err(AmpError::I2c)?;
    i2c.write(&[AUDIO_PROC_REG, desired])
        .map_err(AmpError::I2c)?;
    let verify = read_audio_proc_reg_blocking(i2c)?;
    if is_muted_bit(verify) != muted {
        return Err(AmpError::internal(format!(
            "serial mute verification failed: wrote 0x{desired:02X}, read back 0x{verify:02X}"
        )));
    }
    Ok(())
}
