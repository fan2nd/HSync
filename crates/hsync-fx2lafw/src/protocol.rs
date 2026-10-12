// SPDX-License-Identifier: GPL-3.0-or-later
// Protocol references copyright (C) 2010-2013 Bert Vermeulen <bert@biot.com>
// and copyright (C) 2012 Joel Holdsworth <joel@airwebreathe.org.uk>.
//! Wire protocol ported from libsigrok's fx2lafw/protocol.c and ezusb.c.

use hsync_driver::{DriverError, usb::UsbTransport};
use std::time::Duration;

pub(crate) const TIMEOUT: Duration = Duration::from_millis(100);
pub(crate) const RATES: [u64; 16] = [
    20_000, 25_000, 50_000, 100_000, 200_000, 250_000, 500_000, 1_000_000, 2_000_000, 3_000_000,
    4_000_000, 6_000_000, 8_000_000, 12_000_000, 16_000_000, 24_000_000,
];

pub(crate) fn start_bytes(rate: u64) -> Result<[u8; 3], DriverError> {
    if !RATES.contains(&rate) {
        return Err(DriverError::Unsupported);
    }
    for (clock, flags) in [(48_000_000, 0x40), (30_000_000, 0)] {
        if clock % rate == 0 {
            let delay = clock / rate - 1;
            if (1..=1536).contains(&delay) {
                return Ok([flags, (delay >> 8) as u8, delay as u8]);
            }
        }
    }
    Err(DriverError::Unsupported)
}

pub(crate) fn version(usb: &dyn UsbTransport) -> Result<String, DriverError> {
    let version = usb.control_in(0xb0, 0, 0, 2, TIMEOUT)?;
    match version.as_slice() {
        [1, minor] => Ok(format!("1.{minor}")),
        [major, minor] => Err(DriverError::FirmwareIncompatible(format!(
            "{major}.{minor}"
        ))),
        _ => Err(DriverError::Fault),
    }
}

pub(crate) fn upload(usb: &dyn UsbTransport, image: &[u8]) -> Result<(), DriverError> {
    // FX2 internal code RAM ends at 0x3fff. Reject empty/truncated-as-empty or oversized
    // resources before the CPUCS write. These are flat binary images, not Intel HEX.
    if image.is_empty() || image.len() > 0x4000 {
        return Err(DriverError::Fault);
    }
    usb.control_out(0xa0, 0xe600, 0, &[1], TIMEOUT)?;
    for (block, data) in image.chunks(4096).enumerate() {
        // On failure leave the CPU halted; unplugging restores the EEPROM firmware.
        // Starting a partially uploaded image could execute arbitrary stale RAM.
        usb.control_out(0xa0, (block * 4096) as u16, 0, data, TIMEOUT)?;
    }
    usb.control_out(0xa0, 0xe600, 0, &[0], TIMEOUT)?;
    Ok(())
}
