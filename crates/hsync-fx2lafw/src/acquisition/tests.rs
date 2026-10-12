// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use hsync_driver::mock::{MockDevice, MockTransport, UsbCall};
use hsync_driver::usb::UsbError;
use hsync_driver::{Candidate, CandidateStatus, Device, OptionKey, OptionValue};

#[derive(Default)]
struct Fixture {
    data: VecDeque<Vec<u8>>,
    fail_start: bool,
    detached: bool,
    cancelled: usize,
    version: Vec<u8>,
    fail_upload: bool,
}
impl MockDevice for Fixture {
    fn control_out(&mut self, request: u8, value: u16, _: u16, _: &[u8]) -> Result<(), UsbError> {
        if (self.fail_start && request == 0xb1)
            || (self.fail_upload && request == 0xa0 && value == 0)
        {
            Err(UsbError::Timeout)
        } else {
            Ok(())
        }
    }
    fn control_in(&mut self, _: u8, _: u16, _: u16, _: u16) -> Result<Vec<u8>, UsbError> {
        Ok(self.version.clone())
    }
    fn bulk_out(&mut self, _: u8, _: &[u8]) -> Result<(), UsbError> {
        panic!("no bulk writes")
    }
    fn bulk_in(&mut self, endpoint: u8, _: usize) -> Option<Result<Vec<u8>, UsbError>> {
        assert_eq!(endpoint, 0x82);
        if self.detached {
            Some(Err(nusb::transfer::TransferError::Disconnected.into()))
        } else {
            self.data.pop_front().map(Ok)
        }
    }
    fn cancel_in(&mut self, _: u8, _: usize) -> Vec<u8> {
        self.cancelled += 1;
        vec![]
    }
}
fn params() -> Params {
    Params {
        rate: 1_000_000,
        limit: 128,
        channels: vec![0, 3, 7],
        plan: CapturePlan::default(),
    }
}
fn unpack(bytes: &[u8], channels: usize) -> Vec<u8> {
    let mut out = vec![];
    for group in bytes.chunks_exact(channels * 8) {
        for bit in 0..64 {
            let mut sample = 0;
            for ch in 0..channels {
                let word = u64::from_le_bytes(group[ch * 8..ch * 8 + 8].try_into().expect("word"));
                sample |= (((word >> bit) & 1) as u8) << ch;
            }
            out.push(sample);
        }
    }
    out
}
#[test]
fn every_rate_has_the_reference_command() {
    let expected = [
        [0, 5, 0xdb],
        [0, 4, 0xaf],
        [0x40, 3, 0xbf],
        [0x40, 1, 0xdf],
        [0x40, 0, 0xef],
        [0x40, 0, 0xbf],
        [0x40, 0, 0x5f],
        [0x40, 0, 47],
        [0x40, 0, 23],
        [0x40, 0, 15],
        [0x40, 0, 11],
        [0x40, 0, 7],
        [0x40, 0, 5],
        [0x40, 0, 3],
        [0x40, 0, 2],
        [0x40, 0, 1],
    ];
    for (rate, bytes) in protocol::RATES.into_iter().zip(expected) {
        assert_eq!(protocol::start_bytes(rate).expect("rate"), bytes);
    }
    for rate in [0, 1, 48_000_000, u64::MAX] {
        assert!(protocol::start_bytes(rate).is_err());
    }
}
#[test]
fn firmware_upload_halts_chunks_and_restarts() {
    let usb = MockTransport::new(Fixture::default());
    let image = vec![0xa5; 8120];
    protocol::upload(&usb, &image).expect("upload");
    let write = |value, data| UsbCall::ControlOut {
        request: 0xa0,
        value,
        index: 0,
        data,
    };
    assert_eq!(
        usb.calls(),
        vec![
            write(0xe600, vec![1]),
            write(0, vec![0xa5; 4096]),
            write(4096, vec![0xa5; 4024]),
            write(0xe600, vec![0])
        ]
    );
    usb.clear_calls();
    for image in [vec![], vec![0; 0x4001]] {
        assert!(protocol::upload(&usb, &image).is_err());
    }
    assert!(usb.calls().is_empty());
    usb.device().fail_upload = true;
    assert!(protocol::upload(&usb, &[1; 512]).is_err());
    assert_eq!(usb.calls().len(), 2, "partial image must never be started");
}
#[test]
fn checks_version_length_and_major() {
    for (version, valid) in [
        (vec![1, 0], true),
        (vec![1, 9], true),
        (vec![2, 0], false),
        (vec![], false),
        (vec![1], false),
        (vec![1, 0, 0], false),
    ] {
        let usb = MockTransport::new(Fixture {
            version,
            ..Fixture::default()
        });
        assert_eq!(protocol::version(&usb).is_ok(), valid);
        assert_eq!(
            usb.calls(),
            vec![UsbCall::ControlIn {
                request: 0xb0,
                value: 0,
                index: 0,
                length: 2
            }]
        );
    }
}
#[test]
fn packing_and_limits_do_not_depend_on_transfer_boundaries() {
    let data: Vec<_> = (0..=255).collect();
    let expected: Vec<_> = data[..128]
        .iter()
        .map(|v| (v & 1) | ((v >> 2) & 2) | ((v >> 5) & 4))
        .collect();
    for size in [1, 3, 63, 64, 65, 127, 256] {
        let mut samples = Samples::new(&params());
        let mut bytes = vec![];
        let mut triggers = vec![];
        for chunk in data.chunks(size) {
            let (trigger, cross) = samples.push(chunk);
            triggers.extend(trigger);
            bytes.extend(cross);
        }
        assert_eq!(triggers, vec![0]);
        assert_eq!(unpack(&bytes, 3), expected);
        assert!(samples.complete());
    }
}
#[test]
fn pretrigger_wrap_and_edge_across_transfers() {
    let mut p = params();
    p.channels = vec![0, 1];
    p.plan.trigger_position = 50;
    p.plan.triggers = vec![(0, TriggerCondition::Rise), (1, TriggerCondition::High)];
    let data: Vec<_> = (0..300).map(|n| if n >= 173 { 3 } else { 2 }).collect();
    for size in [1, 7, 64, 173, 300] {
        let mut samples = Samples::new(&p);
        let mut bytes = vec![];
        let mut triggers = vec![];
        for chunk in data.chunks(size) {
            let (trigger, cross) = samples.push(chunk);
            triggers.extend(trigger);
            bytes.extend(cross);
            assert!(samples.history.len() <= 64);
        }
        assert_eq!(triggers, vec![64]);
        assert_eq!(unpack(&bytes, 2), data[109..237]);
    }
}
#[test]
fn trigger_truth_table_and_first_sample_edges() {
    for condition in TriggerCondition::ALL {
        for old in [None, Some(0), Some(1)] {
            for new in [0, 1] {
                let mut p = params();
                p.plan.triggers = vec![(0, condition)];
                let mut samples = Samples::new(&p);
                samples.previous = old;
                let expected = match condition {
                    TriggerCondition::None => true,
                    TriggerCondition::Low => new == 0,
                    TriggerCondition::High => new == 1,
                    TriggerCondition::Rise => old == Some(0) && new == 1,
                    TriggerCondition::Fall => old == Some(1) && new == 0,
                    TriggerCondition::Both => old.is_some_and(|v| v != new),
                };
                assert_eq!(samples.matches(new), expected);
            }
        }
    }
}
#[test]
fn early_trigger_uses_only_available_history_and_instant_ignores_conditions() {
    let mut p = params();
    p.plan.trigger_position = 90;
    p.plan.triggers = vec![(0, TriggerCondition::Rise)];
    let mut samples = Samples::new(&p);
    assert_eq!(samples.push(&[0, 1]).0, Some(1));
    p.plan.instant = true;
    assert_eq!(Samples::new(&p).push(&[0; 128]).0, Some(0));
    p.plan.looping = true;
    let mut samples = Samples::new(&p);
    assert_eq!(unpack(&samples.push(&[0; 256]).1, 3).len(), 256);
    assert!(!samples.complete());
}
#[test]
fn worker_completes_cancels_and_can_run_again() {
    let usb = Arc::new(MockTransport::new(Fixture::default()));
    let running = Arc::new(AtomicBool::new(false));
    for _ in 0..2 {
        usb.device().data = VecDeque::from([vec![0x81; 512]]);
        let (tx, rx) = crossbeam_channel::unbounded();
        start(usb.clone(), running.clone(), params(), tx)
            .expect("start")
            .wait();
        let packets: Vec<_> = rx.try_iter().collect();
        assert!(matches!(packets[0], Packet::Start { .. }));
        assert_eq!(packets[1], Packet::Trigger { sample: 0 });
        let Packet::Logic(data) = &packets[2] else {
            panic!("logic");
        };
        assert_eq!(unpack(data, 3), vec![5; 128]);
        assert_eq!(packets.last(), Some(&Packet::End(EndReason::Completed)));
        assert!(!running.load(Ordering::Acquire));
    }
    assert_eq!(usb.device().cancelled, 64);
    let calls = usb.calls();
    assert!(calls[..32].iter().all(|call| *call
        == UsbCall::BulkInSubmit {
            endpoint: 0x82,
            length: 10240
        }));
    assert_eq!(
        calls[32],
        UsbCall::ControlOut {
            request: 0xb1,
            value: 0,
            index: 0,
            data: vec![0x40, 0, 47]
        }
    );
}
#[test]
fn stopping_while_waiting_and_usb_failures_end_the_capture() {
    for (fail_start, detached) in [(false, false), (true, false), (false, true)] {
        let usb = Arc::new(MockTransport::new(Fixture {
            fail_start,
            detached,
            ..Fixture::default()
        }));
        let running = Arc::new(AtomicBool::new(false));
        let (tx, rx) = crossbeam_channel::unbounded();
        let capture = start(usb.clone(), running.clone(), params(), tx).expect("start");
        if !fail_start && !detached {
            capture.stop();
        }
        capture.wait();
        let packets: Vec<_> = rx.try_iter().collect();
        let Packet::End(reason) = packets.last().expect("end") else {
            panic!("end packet");
        };
        if fail_start {
            assert!(matches!(reason, EndReason::Failed(_)));
        } else if detached {
            assert_eq!(*reason, EndReason::Detached);
        } else {
            assert_eq!(*reason, EndReason::Stopped);
        }
        assert!(!running.load(Ordering::Acquire));
        assert!(usb.device().cancelled > 0);
    }
}
#[test]
fn options_reject_unsupported_requests_without_usb_io() {
    let usb = Arc::new(MockTransport::new(Fixture::default()));
    let candidate = Candidate {
        id: "test".into(),
        driver: "fx2lafw",
        vendor: "USB Logic".into(),
        model: "Logic 8 / 24 MHz".into(),
        status: CandidateStatus::Ready,
        usb: None,
    };
    let mut device = crate::device::Fx2Device::new(usb.clone(), &candidate, "1.0".into());
    assert_eq!(device.channel_count(), 8);
    assert!(device.can_loop());
    assert!(!device.trigger_caps().advanced);
    for (key, value) in [
        (OptionKey::Samplerate, OptionValue::Integer(48_000_000)),
        (OptionKey::SampleLimit, OptionValue::Integer(u64::MAX)),
        (OptionKey::SampleLimit, OptionValue::Integer(0)),
        (
            OptionKey::OperationMode,
            OptionValue::Choice("buffer".into()),
        ),
    ] {
        assert!(device.set_option(key, &value).is_err());
    }
    device
        .set_option(OptionKey::SampleLimit, &OptionValue::Integer(1025))
        .expect("round limit");
    assert_eq!(device.options()[2].value, OptionValue::Integer(1024));
    assert!(device.set_channel_enabled(8, false).is_err());
    for ch in 0..8 {
        device.set_channel_enabled(ch, false).expect("disable");
    }
    let (tx, _) = crossbeam_channel::unbounded();
    assert!(device.start(CapturePlan::default(), tx).is_err());
    assert!(usb.calls().is_empty());
}
