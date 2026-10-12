// SPDX-License-Identifier: GPL-3.0-or-later
//! FX2 bulk acquisition (libsigrok fx2lafw protocol); HSync software triggering and packing.

use std::collections::VecDeque;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::protocol;
use crossbeam_channel::{SendTimeoutError, Sender};
use hsync_core::{ChannelIndex, TriggerCondition, UiError};
use hsync_driver::usb::{BulkIn, UsbTransport};
use hsync_driver::{Acquisition, CapturePlan, DriverError, EndReason, Packet};

const POLL: Duration = Duration::from_millis(20);
const EP: u8 = 0x82;

pub(crate) struct Params {
    pub rate: u64,
    pub limit: u64,
    pub channels: Vec<ChannelIndex>,
    pub plan: CapturePlan,
}

pub(crate) fn start(
    usb: Arc<dyn UsbTransport>,
    running: Arc<AtomicBool>,
    params: Params,
    sink: Sender<Packet>,
) -> Result<Box<dyn Acquisition>, DriverError> {
    // About 10 ms per transfer, 32 transfers in flight, matching fx2lafw's USB pipeline.
    let size = (params.rate as usize / 100).div_ceil(512) * 512;
    let bulk = usb.bulk_in(EP)?;
    let command = protocol::start_bytes(params.rate)?;
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread_running = Arc::clone(&running);
    running.store(true, Ordering::Release);
    // Put setup in the worker: failing to spawn cannot leave a sampling device or queued USB IO.
    let result = std::thread::Builder::new()
        .name("fx2lafw-acquisition".into())
        .spawn(move || {
            let mut worker = Worker {
                bulk,
                usb,
                size,
                sink,
                stop: thread_stop,
            };
            let reason = worker.run(params, command);
            // fx2lafw has no STOP request. Like libsigrok, cancel and drain all queued reads.
            // The next START resets the GPIF engine and FIFO before sampling again.
            worker.bulk.cancel_all();
            let deadline = Instant::now() + Duration::from_secs(2);
            while worker.bulk.pending() > 0 && Instant::now() < deadline {
                let _ = worker.bulk.next(POLL);
            }
            thread_running.store(false, Ordering::Release);
            // Session consumers are unbounded. A stalled external bounded consumer must not
            // prevent cancellation or keep the acquisition thread alive forever.
            let _ = worker
                .sink
                .send_timeout(Packet::End(reason), Duration::from_secs(1));
        });
    match result {
        Ok(thread) => Ok(Box::new(Fx2Acquisition {
            stop,
            thread: Some(thread),
        })),
        Err(_) => {
            running.store(false, Ordering::Release);
            Err(DriverError::Fault)
        }
    }
}

struct Fx2Acquisition {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Acquisition for Fx2Acquisition {
    fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
    fn wait(mut self: Box<Self>) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
impl Drop for Fx2Acquisition {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

struct Worker {
    bulk: Box<dyn BulkIn>,
    usb: Arc<dyn UsbTransport>,
    size: usize,
    sink: Sender<Packet>,
    stop: Arc<AtomicBool>,
}

fn failed(error: DriverError) -> EndReason {
    EndReason::Failed(UiError::from(&error))
}

impl Worker {
    fn send(&self, mut packet: Packet) -> bool {
        loop {
            match self.sink.send_timeout(packet, POLL) {
                Ok(()) => return true,
                Err(SendTimeoutError::Disconnected(_)) => return false,
                Err(SendTimeoutError::Timeout(value)) => packet = value,
            }
            if self.stop.load(Ordering::Acquire) {
                return false;
            }
        }
    }

    fn run(&mut self, params: Params, command: [u8; 3]) -> EndReason {
        if !self.send(Packet::Start {
            channels: params.channels.clone(),
            samplerate: params.rate,
        }) {
            return EndReason::Stopped;
        }
        // Queue reads before START, as libsigrok does. The firmware starts GPIF on the
        // first IN token after receiving START.
        for _ in 0..32 {
            self.bulk.submit(self.size);
        }
        if let Err(error) = self
            .usb
            .control_out(0xb1, 0, 0, &command, protocol::TIMEOUT)
        {
            return failed(error.into());
        }
        let mut samples = Samples::new(&params);
        let mut last_data = Instant::now();
        loop {
            if self.stop.load(Ordering::Acquire) {
                return EndReason::Stopped;
            }
            match self.bulk.next(POLL) {
                Some(Err(error)) => {
                    return if error.is_disconnected() {
                        EndReason::Detached
                    } else {
                        failed(error.into())
                    };
                }
                Some(Ok(data)) => {
                    if !data.is_empty() {
                        last_data = Instant::now();
                    }
                    // Replenish the USB pipeline before converting samples on the CPU.
                    self.bulk.submit(self.size);
                    for chunk in data.chunks(4096) {
                        if self.stop.load(Ordering::Acquire) {
                            return EndReason::Stopped;
                        }
                        let (trigger, cross) = samples.push_cancellable(chunk, &self.stop);
                        if self.stop.load(Ordering::Acquire) {
                            return EndReason::Stopped;
                        }
                        if let Some(sample) = trigger
                            && !self.send(Packet::Trigger { sample })
                        {
                            return EndReason::Stopped;
                        }
                        if !cross.is_empty() && !self.send(Packet::Logic(cross)) {
                            return EndReason::Stopped;
                        }
                        if samples.complete() {
                            return EndReason::Completed;
                        }
                    }
                }
                None => {}
            }
            // Even with a static input, fx2lafw sends samples. Waiting for a software
            // trigger is unlimited, but a silent USB device gets a bounded timeout.
            if last_data.elapsed() > Duration::from_secs(3) {
                return failed(DriverError::Usb(hsync_driver::usb::UsbError::Timeout));
            }
        }
    }
}

struct Samples {
    channels: Vec<ChannelIndex>,
    triggers: Vec<(ChannelIndex, TriggerCondition)>,
    previous: Option<u8>,
    // At most limit * position / 100 bytes; limit is bounded to 64 Mi samples.
    history: VecDeque<u8>,
    pre: usize,
    fired: bool,
    count: u64,
    limit: Option<u64>,
    words: [u64; 8],
    bits: u32,
}

impl Samples {
    fn new(params: &Params) -> Self {
        let triggers = if params.plan.instant {
            vec![]
        } else {
            params
                .plan
                .triggers
                .iter()
                .copied()
                .filter(|(_, condition)| *condition != TriggerCondition::None)
                .collect()
        };
        Self {
            channels: params.channels.clone(),
            triggers,
            previous: None,
            history: VecDeque::new(),
            pre: (params.limit * u64::from(params.plan.trigger_position) / 100) as usize,
            fired: false,
            count: 0,
            limit: (!params.plan.looping).then_some(params.limit),
            words: [0; 8],
            bits: 0,
        }
    }
    fn matches(&self, value: u8) -> bool {
        self.triggers.iter().all(|&(channel, condition)| {
            let high = value & (1 << channel) != 0;
            let previous = self.previous.map(|v| v & (1 << channel) != 0);
            match condition {
                TriggerCondition::None => true,
                TriggerCondition::Low => !high,
                TriggerCondition::High => high,
                TriggerCondition::Rise => high && previous == Some(false),
                TriggerCondition::Fall => !high && previous == Some(true),
                TriggerCondition::Both => previous.is_some_and(|old| old != high),
            }
        })
    }
    fn complete(&self) -> bool {
        self.limit.is_some_and(|limit| self.count >= limit)
    }
    fn append(&mut self, value: u8, out: &mut Vec<u8>) {
        for &channel in &self.channels {
            self.words[channel as usize] |= u64::from((value >> channel) & 1) << self.bits;
        }
        self.bits += 1;
        self.count += 1;
        if self.bits == 64 {
            for &channel in &self.channels {
                out.extend_from_slice(&self.words[channel as usize].to_le_bytes());
            }
            self.words = [0; 8];
            self.bits = 0;
        }
    }
    #[cfg(test)]
    fn push(&mut self, data: &[u8]) -> (Option<u64>, Vec<u8>) {
        self.push_cancellable(data, &AtomicBool::new(false))
    }
    fn push_cancellable(&mut self, data: &[u8], stop: &AtomicBool) -> (Option<u64>, Vec<u8>) {
        let mut trigger = None;
        let mut cross = Vec::new();
        for &value in data {
            if self.complete() {
                break;
            }
            if !self.fired && self.matches(value) {
                self.fired = true;
                trigger = Some(self.history.len() as u64);
                while let Some(sample) = self.history.pop_front() {
                    if self.count.is_multiple_of(4096) && stop.load(Ordering::Acquire) {
                        return (trigger, cross);
                    }
                    self.append(sample, &mut cross);
                }
            }
            if self.fired {
                self.append(value, &mut cross);
            } else if self.pre != 0 {
                if self.history.len() == self.pre {
                    self.history.pop_front();
                }
                self.history.push_back(value);
            }
            self.previous = Some(value);
        }
        (trigger, cross)
    }
}

#[cfg(test)]
mod tests;
