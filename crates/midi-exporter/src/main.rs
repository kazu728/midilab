//! Tail the capture log and push OTLP metrics — a derived view of the JSONL
//! truth, deliberately separate from the capture daemon so that no exporter
//! failure can ever cost captured data.

mod metrics;
mod tail;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::Local;
use clap::Parser;
use opentelemetry::metrics::MeterProvider;
use opentelemetry_otlp::MetricExporter;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};

use metrics::PianoMetrics;
use tail::Tail;

const POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Parser)]
#[command(about = "Export OTLP metrics derived from the midilogd capture log")]
struct Args {
    /// Capture directory to tail (midilogd's --capture-dir).
    #[arg(long, default_value = "capture")]
    capture_dir: PathBuf,
    /// Silence gap that starts a new session, in seconds.
    #[arg(long, default_value_t = 300)]
    gap_secs: u64,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<(), String> {
    let shutdown = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        signal_hook::flag::register(signal, Arc::clone(&shutdown))
            .map_err(|e| format!("installing signal handler: {e}"))?;
    }

    // Endpoint resolves from OTEL_EXPORTER_OTLP_[METRICS_]ENDPOINT, or the
    // SDK's localhost default when unset.
    let exporter = MetricExporter::builder()
        .with_http()
        .build()
        .map_err(|e| format!("building OTLP exporter: {e}"))?;
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter).build())
        // Built empty: the collector flattens every resource attribute onto
        // each series, so the default detectors would only add noise labels.
        .with_resource(
            Resource::builder_empty()
                .with_service_name("midi-exporter")
                .build(),
        )
        .build();
    let mut piano = PianoMetrics::new(
        &provider.meter("midi-exporter"),
        Duration::from_secs(args.gap_secs),
    );

    // The log is partitioned by the capture host's local date, so the runtime
    // timezone must match it (set TZ accordingly); otherwise the day rollover
    // lags until UTC midnight and events go unseen until then.
    let mut tail = Tail::new(args.capture_dir.clone(), Local::now().date_naive())
        .map_err(|e| format!("attaching to {}: {e}", args.capture_dir.display()))?;

    while !shutdown.load(Ordering::Relaxed) {
        let events = tail
            .poll(Local::now().date_naive())
            .map_err(|e| format!("reading {}: {e}", args.capture_dir.display()))?;
        for event in &events {
            piano.observe(event);
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    provider
        .shutdown()
        .map_err(|e| format!("flushing metrics: {e}"))
}
