// SPDX-License-Identifier: GPL-3.0-or-later
//! Device capabilities and validated acquisition settings.

use crate::{acquisition, protocol};
use crossbeam_channel::Sender;
use hsync_core::ChannelIndex;
use hsync_driver::usb::UsbTransport;
use hsync_driver::{
    Acquisition, Candidate, CapturePlan, Choice, Device, DeviceInfo, DriverError, Label, OptionKey,
    OptionKind, OptionState, OptionValue, Packet,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

// A one-byte-per-sample pre-trigger ring is bounded by this capture depth. Samples
// already emitted belong to the session, which also bounds its loop capture history.
const MAX_SAMPLES: u64 = 64 * 1024 * 1024;

pub(crate) struct Fx2Device {
    usb: Arc<dyn UsbTransport>,
    info: DeviceInfo,
    rate: u64,
    limit: u64,
    enabled: u8,
    running: Arc<AtomicBool>,
}

impl Fx2Device {
    pub(crate) fn new(usb: Arc<dyn UsbTransport>, candidate: &Candidate, version: String) -> Self {
        Self {
            usb,
            info: DeviceInfo {
                id: candidate.id.clone(),
                driver: "fx2lafw".into(),
                vendor: candidate.vendor.clone(),
                model: candidate.model.clone(),
                firmware: Some(version),
                usb_speed: Some("high".into()),
                slow_port: false,
            },
            rate: 1_000_000,
            limit: 1_000_000,
            enabled: 0xff,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    fn idle(&self) -> Result<(), DriverError> {
        if self.running.load(Ordering::Acquire) {
            Err(DriverError::Busy)
        } else {
            Ok(())
        }
    }
}

impl Device for Fx2Device {
    fn info(&self) -> DeviceInfo {
        self.info.clone()
    }

    fn options(&self) -> Vec<OptionState> {
        vec![
            OptionState {
                key: OptionKey::OperationMode,
                kind: OptionKind::Choice {
                    choices: vec![Choice {
                        id: "stream".into(),
                        label: Label::new("device.operationMode.stream"),
                    }],
                },
                value: OptionValue::Choice("stream".into()),
                enabled: false,
            },
            OptionState {
                key: OptionKey::Samplerate,
                kind: OptionKind::Integers {
                    values: protocol::RATES.to_vec(),
                },
                value: OptionValue::Integer(self.rate),
                enabled: true,
            },
            OptionState {
                key: OptionKey::SampleLimit,
                kind: OptionKind::Range {
                    min: 1024.0,
                    max: MAX_SAMPLES as f64,
                    step: 64.0,
                },
                value: OptionValue::Integer(self.limit),
                enabled: true,
            },
        ]
    }

    fn set_option(&mut self, key: OptionKey, value: &OptionValue) -> Result<(), DriverError> {
        self.idle()?;
        match (key, value) {
            (OptionKey::OperationMode, OptionValue::Choice(mode)) if mode == "stream" => {}
            (OptionKey::Samplerate, OptionValue::Integer(rate))
                if protocol::RATES.contains(rate) =>
            {
                self.rate = *rate;
            }
            (OptionKey::SampleLimit, OptionValue::Integer(limit))
                if (1024..=MAX_SAMPLES).contains(limit) =>
            {
                // Cross-format storage has 64 samples per group, as in other hardware drivers.
                self.limit = limit / 64 * 64;
            }
            _ => return Err(DriverError::Unsupported),
        }
        Ok(())
    }

    fn channel_count(&self) -> u16 {
        8
    }
    fn can_loop(&self) -> bool {
        true
    }
    fn enabled_channels(&self) -> Vec<ChannelIndex> {
        (0..8)
            .filter(|channel| self.enabled & (1 << channel) != 0)
            .collect()
    }
    fn set_channel_enabled(
        &mut self,
        channel: ChannelIndex,
        enabled: bool,
    ) -> Result<(), DriverError> {
        self.idle()?;
        if channel >= 8 {
            return Err(DriverError::Unsupported);
        }
        if enabled {
            self.enabled |= 1 << channel;
        } else {
            self.enabled &= !(1 << channel);
        }
        Ok(())
    }
    fn start(
        &mut self,
        plan: CapturePlan,
        sink: Sender<Packet>,
    ) -> Result<Box<dyn Acquisition>, DriverError> {
        self.idle()?;
        if self.enabled == 0
            || (!plan.instant
                && (plan.advanced.is_some()
                    || plan.trigger_position > 90
                    || plan
                        .triggers
                        .iter()
                        .any(|(ch, _)| *ch >= 8 || self.enabled & (1 << ch) == 0)))
        {
            return Err(DriverError::Unsupported);
        }
        acquisition::start(
            Arc::clone(&self.usb),
            Arc::clone(&self.running),
            acquisition::Params {
                rate: self.rate,
                limit: self.limit,
                channels: self.enabled_channels(),
                plan,
            },
            sink,
        )
    }
}
