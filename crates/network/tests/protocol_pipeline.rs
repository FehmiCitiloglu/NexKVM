use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use coklu_input::{AdaptivePoller, InputBatchPolicy, InputBatcher, InputEvent, PollingPolicy};
use coklu_network::{
    NetworkQualityEstimator, NetworkQualityGrade, NetworkQualitySample, ZeroCopyPacket,
};
use coklu_protocol::{Envelope, FrameCodec, MessageId, MessageKind, PROTOCOL_VERSION};
use coklu_streaming::{TransferCompression, TransferCompressionPolicy};

#[test]
fn framed_zero_copy_packet_round_trips_through_protocol_pipeline() {
    let envelope = Envelope::new(
        PROTOCOL_VERSION,
        MessageId(99),
        MessageKind::Input,
        Bytes::from_static(b"batched-input"),
    );
    let packet = ZeroCopyPacket::from_envelope(&envelope);

    let mut framed = BytesMut::new();
    FrameCodec.encode(packet.bytes(), &mut framed).unwrap();

    let payload = FrameCodec.decode(&mut framed).unwrap().unwrap();
    let decoded = ZeroCopyPacket::from_bytes(payload).decode().unwrap();

    assert_eq!(decoded.id, envelope.id);
    assert_eq!(decoded.kind, envelope.kind);
    assert_eq!(decoded.body, envelope.body);
    assert!(framed.is_empty());
}

#[test]
fn quality_sample_drives_input_latency_primitives() {
    let mut quality = NetworkQualityEstimator::new(1.0);
    let recommendation = quality.record(NetworkQualitySample {
        rtt: Duration::from_millis(12),
        jitter: Duration::from_millis(2),
        loss: 0.0,
        throughput_bps: 50_000_000,
    });
    assert_eq!(recommendation.grade, NetworkQualityGrade::Excellent);

    let mut batcher = InputBatcher::new(InputBatchPolicy::balanced());
    batcher.update_rtt(quality.rtt());
    let now = Instant::now();
    batcher.push(InputEvent::RelativeMove { dx: 1.0, dy: 0.0 }, now);
    assert!(!batcher.should_flush(now));
    assert!(batcher.should_flush(now + Duration::from_millis(2)));

    let mut poller = AdaptivePoller::new(PollingPolicy::default());
    poller.record_activity(now);
    poller.apply_network_pressure(quality.jitter().unwrap(), 0.0);
    assert_eq!(
        poller.interval(now + Duration::from_millis(10)),
        Duration::from_millis(1)
    );
}

#[test]
fn streaming_compression_policy_exposes_latency_and_throughput_modes() {
    let latency = TransferCompressionPolicy::latency_first();
    let throughput = TransferCompressionPolicy::throughput_first();

    assert_eq!(latency.choose(1024), TransferCompression::None);
    assert_eq!(throughput.choose(1024), throughput.preferred);
}
