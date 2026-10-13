// SPDX-License-Identifier: GPL-3.0-or-later
//! Eight-channel FX2 logic analyzers using the sigrok fx2lafw protocol.
//!
//! Protocol reference: libsigrok's `src/hardware/fx2lafw` and `src/ezusb.c`
//! (GPL-3.0-or-later). All host USB access is implemented in Rust through nusb.

#![forbid(unsafe_code)]

mod acquisition;
mod device;
mod protocol;

use std::sync::Arc;

use hsync_driver::usb::NusbTransport;
use hsync_driver::{Candidate, CandidateStatus, Device, Driver, DriverError, FirmwareDir};

/// Driver for Saleae Logic compatible USB Logic 8-channel / 24 MHz analyzers.
#[derive(Debug)]
pub struct Fx2lafwDriver {
    firmware: FirmwareDir,
}

impl Fx2lafwDriver {
    /// Uses the application's firmware directory, containing the `fx2lafw` subdirectory.
    pub fn new(firmware: FirmwareDir) -> Self {
        Self { firmware }
    }
}

fn profile(vid: u16, pid: u16) -> Option<(&'static str, &'static str)> {
    match (vid, pid) {
        (0x0925, 0x3881) => Some(("USB Logic", "fx2lafw/fx2lafw-saleae-logic.fw")),
        (0x1d50, 0x608c) => Some(("sigrok", "fx2lafw/fx2lafw-sigrok-fx2-8ch.fw")),
        // The generic Cypress ID also identifies development boards and 16-channel devices.
        // Never upload an eight-channel firmware to it based on the chip alone.
        _ => None,
    }
}

impl Driver for Fx2lafwDriver {
    fn name(&self) -> &'static str {
        "fx2lafw"
    }

    fn identify(&self, usb: &nusb::DeviceInfo) -> Option<Candidate> {
        let (vendor, _) = profile(usb.vendor_id(), usb.product_id())?;
        let ports: Vec<_> = usb.port_chain().iter().map(u8::to_string).collect();
        // Unlike libsigrok's explicit string-descriptor reads, nusb enumeration
        // has no cached manufacturer string on Windows (it always returns None).
        // Require the firmware's product name, but only check the manufacturer
        // when available. Opening still verifies the firmware's protocol version.
        // Otherwise every Windows scan classifies loaded firmware as missing,
        // and open_after_restart can never find a Ready device.
        let loaded = usb
            .product_string()
            .is_some_and(|s| s.starts_with("fx2lafw"))
            && usb
                .manufacturer_string()
                .is_none_or(|s| s.starts_with("sigrok"));
        let status = if matches!(usb.speed(), Some(nusb::Speed::Low | nusb::Speed::Full)) {
            CandidateStatus::SpeedTooLow
        } else if loaded {
            CandidateStatus::Ready
        } else {
            CandidateStatus::NeedsFirmware
        };
        Some(Candidate {
            id: format!("fx2lafw:{}-{}", usb.bus_id(), ports.join(".")),
            driver: self.name(),
            vendor: vendor.into(),
            model: "Logic 8 / 24 MHz".into(),
            status,
            usb: Some(usb.clone()),
        })
    }

    fn open(&self, candidate: &Candidate) -> Result<Box<dyn Device>, DriverError> {
        let usb = candidate.usb.as_ref().ok_or(DriverError::Unsupported)?;
        let (_, firmware) =
            profile(usb.vendor_id(), usb.product_id()).ok_or(DriverError::Unsupported)?;
        match candidate.status {
            CandidateStatus::NeedsFirmware => {
                // Read and validate before halting the CPU. Firmware lives only in RAM;
                // the EEPROM and the device's original identity are left untouched.
                let image = self.firmware.load(firmware)?;
                let transport =
                    NusbTransport::open_configured(usb, 1, 0).map_err(DriverError::opening)?;
                protocol::upload(&transport, &image)?;
                Err(DriverError::Restarting)
            }
            CandidateStatus::Ready => {
                let transport = NusbTransport::open(usb, 1, 0).map_err(DriverError::opening)?;
                let version = protocol::version(&transport)?;
                Ok(Box::new(device::Fx2Device::new(
                    Arc::new(transport),
                    candidate,
                    version,
                )))
            }
            _ => Err(DriverError::Unsupported),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_eight_channel_profiles() {
        assert!(profile(0x0925, 0x3881).is_some());
        assert!(profile(0x1d50, 0x608c).is_some());
        for (vid, pid) in [(0x04b4, 0x8613), (0x1d50, 0x608d), (0x2a0e, 0x0020)] {
            assert!(profile(vid, pid).is_none());
        }
    }
}
