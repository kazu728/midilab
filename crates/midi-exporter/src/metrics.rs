//! Project captured events onto the three OTel instruments.
//!
//! Only `note_on` with a non-zero velocity counts as a keystroke (`vel == 0`
//! is the running-status note-off convention). Session time follows the same
//! silence-gap rule as [`midi_event::sessions`], applied incrementally: each
//! inter-event gap of at most `gap` is added as played time, so the counter's
//! per-session total equals that view's last-minus-first span. A backwards
//! monotonic step (reboot) is a boundary there too, and adds nothing.

use std::time::Duration;

use midi_event::{Event, Message};
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter};

/// Eight equal bands across the 7-bit range, pp through ff.
const VELOCITY_BOUNDARIES: [f64; 7] = [16.0, 32.0, 48.0, 64.0, 80.0, 96.0, 112.0];

pub struct PianoMetrics {
    keys: Counter<u64>,
    velocity: Histogram<f64>,
    session: Counter<f64>,
    gap_ns: u64,
    prev_t_mono_ns: Option<u64>,
}

impl PianoMetrics {
    pub fn new(meter: &Meter, gap: Duration) -> Self {
        Self {
            keys: meter.u64_counter("piano.keys").build(),
            velocity: meter
                .f64_histogram("piano.velocity")
                .with_boundaries(VELOCITY_BOUNDARIES.to_vec())
                .build(),
            session: meter.f64_counter("piano.session").with_unit("s").build(),
            gap_ns: gap.as_nanos().min(u64::MAX as u128) as u64,
            prev_t_mono_ns: None,
        }
    }

    pub fn observe(&mut self, event: &Event) {
        if let Some(prev) = self.prev_t_mono_ns
            && let Some(dt) = event.t_mono_ns.checked_sub(prev)
            && dt <= self.gap_ns
        {
            self.session.add(dt as f64 / 1e9, &[]);
        }
        self.prev_t_mono_ns = Some(event.t_mono_ns);

        if let Message::NoteOn { note, vel, .. } = event.msg
            && vel > 0
        {
            // Zero-padded so Grafana's lexicographic label order is pitch order.
            self.keys
                .add(1, &[KeyValue::new("note", format!("{note:03}"))]);
            self.velocity.record(f64::from(vel), &[]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
    use opentelemetry_sdk::metrics::in_memory_exporter::InMemoryMetricExporter;
    use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};

    const S: u64 = 1_000_000_000;

    fn note_on(t_mono_ns: u64, note: u8, vel: u16) -> Event {
        Event {
            t_mono_ns,
            t_wall: "2026-07-11T21:30:00+09:00".into(),
            src: "24:0".into(),
            group: 0,
            msg: Message::NoteOn { ch: 0, note, vel },
        }
    }

    fn note_off(t_mono_ns: u64, note: u8) -> Event {
        Event {
            t_mono_ns,
            t_wall: "2026-07-11T21:30:00+09:00".into(),
            src: "24:0".into(),
            group: 0,
            msg: Message::NoteOff {
                ch: 0,
                note,
                vel: 0,
            },
        }
    }

    /// Run `events` through PianoMetrics and return the flushed export.
    fn observe_all(events: &[Event], gap: Duration) -> Vec<ResourceMetrics> {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        let mut piano = PianoMetrics::new(&provider.meter("test"), gap);
        for event in events {
            piano.observe(event);
        }
        provider.force_flush().unwrap();
        exporter.get_finished_metrics().unwrap()
    }

    fn with_metric<T>(
        export: &[ResourceMetrics],
        name: &str,
        f: impl FnOnce(&AggregatedMetrics) -> T,
    ) -> T {
        let metric = export
            .iter()
            .flat_map(|rm| rm.scope_metrics())
            .flat_map(|sm| sm.metrics())
            .find(|m| m.name() == name)
            .unwrap_or_else(|| panic!("metric {name} not exported"));
        f(metric.data())
    }

    fn u64_sum(export: &[ResourceMetrics], name: &str) -> u64 {
        with_metric(export, name, |data| match data {
            AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                sum.data_points().map(|dp| dp.value()).sum()
            }
            _ => panic!("{name} is not a u64 sum"),
        })
    }

    fn f64_sum(export: &[ResourceMetrics], name: &str) -> f64 {
        with_metric(export, name, |data| match data {
            AggregatedMetrics::F64(MetricData::Sum(sum)) => {
                sum.data_points().map(|dp| dp.value()).sum()
            }
            _ => panic!("{name} is not an f64 sum"),
        })
    }

    #[test]
    fn keystroke_rules() {
        let export = observe_all(
            &[
                note_on(0, 21, 55),
                note_on(S, 21, 60),
                note_on(2 * S, 108, 127),
                note_on(3 * S, 60, 0), // note-off convention: not a keystroke
                note_off(4 * S, 21),
            ],
            Duration::from_secs(300),
        );

        assert_eq!(u64_sum(&export, "piano.keys"), 3);

        // Keys are labeled by zero-padded note number.
        let mut keys = with_metric(&export, "piano.keys", |data| match data {
            AggregatedMetrics::U64(MetricData::Sum(sum)) => sum
                .data_points()
                .map(|dp| {
                    let note = dp
                        .attributes()
                        .find(|kv| kv.key.as_str() == "note")
                        .expect("note attribute")
                        .value
                        .to_string();
                    (note, dp.value())
                })
                .collect::<Vec<_>>(),
            _ => panic!("piano.keys is not a u64 sum"),
        });
        keys.sort();
        assert_eq!(keys, vec![("021".to_string(), 2), ("108".to_string(), 1)]);

        with_metric(&export, "piano.velocity", |data| match data {
            AggregatedMetrics::F64(MetricData::Histogram(hist)) => {
                let dp = hist.data_points().next().expect("one data point");
                assert_eq!(dp.count(), 3);
                assert_eq!(dp.sum(), 55.0 + 60.0 + 127.0);
                // 55 and 60 fall in (48, 64]; 127 in (112, +inf).
                let buckets: Vec<u64> = dp.bucket_counts().collect();
                assert_eq!(buckets, vec![0, 0, 0, 2, 0, 0, 0, 1]);
            }
            _ => panic!("piano.velocity is not an f64 histogram"),
        });
    }

    #[test]
    fn session_gap_and_reboot_rules() {
        let gap = Duration::from_secs(300);
        let export = observe_all(
            &[
                note_on(0, 21, 55),
                note_on(2 * S, 21, 55),   // +2s
                note_on(400 * S, 23, 55), // gap > 300s: new session, no add
                note_on(403 * S, 23, 55), // +3s
                note_on(10 * S, 24, 55),  // backwards = reboot: no add
                note_on(11 * S, 24, 55),  // +1s
            ],
            gap,
        );
        assert_eq!(f64_sum(&export, "piano.session"), 6.0);
    }

    /// The incremental sum must equal the `sessions()` view: the sum over
    /// sessions of (last event time - first event time).
    #[test]
    fn session_seconds_match_sessions_view() {
        let gap = Duration::from_secs(300);
        let events: Vec<Event> = [
            0,
            S,
            5 * S,
            400 * S, // gap boundary
            401 * S,
            420 * S,
            90 * S, // reboot boundary
            95 * S,
        ]
        .iter()
        .map(|&t| note_on(t, 60, 80))
        .collect();

        let expected: f64 = midi_event::sessions(&events, gap)
            .iter()
            .map(|s| (s.last().unwrap().t_mono_ns - s.first().unwrap().t_mono_ns) as f64 / 1e9)
            .sum();

        let export = observe_all(&events, gap);
        assert_eq!(f64_sum(&export, "piano.session"), expected);
    }
}
