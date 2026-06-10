use std::hint::black_box;
use std::time::{Duration, Instant};

use bytes::Bytes;
use nexkvm_input::{CursorSample, InputBatchPolicy, InputBatcher, InputEvent, PredictiveCursor};
use nexkvm_network::{NetworkQualityEstimator, NetworkQualitySample, ZeroCopyPacket};
use nexkvm_protocol::{Envelope, MessageId, MessageKind, PROTOCOL_VERSION};

const ITERS: usize = 100_000;

fn main() {
    bench_zero_copy_packet_encode_decode();
    bench_input_batching();
    bench_network_quality_estimator();
    bench_predictive_cursor();
}

fn report(name: &str, elapsed: Duration, iterations: usize) {
    let nanos = elapsed.as_nanos() as f64 / iterations as f64;
    println!("{name}: {iterations} iterations in {elapsed:?} ({nanos:.1} ns/op)");
}

fn bench_zero_copy_packet_encode_decode() {
    let body = Bytes::from_static(b"input-payload");
    let start = Instant::now();
    for i in 0..ITERS {
        let env = Envelope::new(
            PROTOCOL_VERSION,
            MessageId(i as u64),
            MessageKind::Input,
            body.clone(),
        );
        let packet = ZeroCopyPacket::from_envelope(&env);
        let decoded = packet.decode().unwrap();
        black_box(decoded);
    }
    report("zero_copy_packet_encode_decode", start.elapsed(), ITERS);
}

fn bench_input_batching() {
    let start = Instant::now();
    for _ in 0..ITERS {
        let now = Instant::now();
        let mut batcher = InputBatcher::new(InputBatchPolicy::low_latency());
        batcher.push(InputEvent::RelativeMove { dx: 1.0, dy: 0.0 }, now);
        batcher.push(InputEvent::RelativeMove { dx: 2.0, dy: 1.0 }, now);
        black_box(batcher.drain());
    }
    report("input_batching", start.elapsed(), ITERS);
}

fn bench_network_quality_estimator() {
    let start = Instant::now();
    let mut estimator = NetworkQualityEstimator::default();
    for _ in 0..ITERS {
        black_box(estimator.record(NetworkQualitySample {
            rtt: Duration::from_millis(15),
            jitter: Duration::from_millis(2),
            loss: 0.001,
            throughput_bps: 80_000_000,
        }));
    }
    report("network_quality_estimator", start.elapsed(), ITERS);
}

fn bench_predictive_cursor() {
    let start = Instant::now();
    let t0 = Instant::now();
    let mut predictor = PredictiveCursor::lan_default();
    predictor.push_sample(CursorSample::new(0.1, 0.1, t0));
    predictor.push_sample(CursorSample::new(0.2, 0.2, t0 + Duration::from_millis(8)));
    for i in 0..ITERS {
        black_box(predictor.predict(t0 + Duration::from_micros(i as u64)));
    }
    report("predictive_cursor", start.elapsed(), ITERS);
}
