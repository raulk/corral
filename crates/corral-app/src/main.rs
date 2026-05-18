mod context_menu;
mod control;
mod focus;
mod runtime;
mod strip;
mod theme;
mod tile;
mod tooltip;
#[cfg(target_os = "macos")]
mod window_geom;

use corral_core::trace::{TraceSink, install_sink};
use gpui::{App, Application};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn main() {
    use clap::Parser;

    #[derive(Parser)]
    #[command(about = "Corral — Claude session monitor")]
    struct Args {
        /// Path to a JSONL trace file. Each event is appended as one
        /// `TraceEvent` per line (`corral_core::trace`). Read by the
        /// integration harness; see
        /// docs/internal/todos/headless-integration-testing.md.
        #[arg(long, value_name = "PATH")]
        trace_file: Option<PathBuf>,
    }

    let args = Args::parse();
    init_tracing();
    if let Some(path) = args.trace_file {
        install_trace_file_sink(&path);
    }

    let mut rt = runtime::bootstrap();
    let events = rt.take_events();
    let control_socket = control::default_socket_path();
    // Failing to bind the control socket is non-fatal: the UI still works,
    // but the integration harness can't drive this corral.
    let control = match control::spawn(control_socket.clone(), rt.sys_tx()) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %control_socket.display(),
                "control socket unavailable; harness control disabled",
            );
            None
        }
    };
    Application::new().run(move |cx: &mut App| {
        strip::open(cx, events);
        // Keep the runtime + control server alive for the lifetime of the
        // app. GPUI's run loop owns the closure; storing them in globals
        // pins driver threads, the FSEvents debouncer, and the control
        // listener.
        cx.set_global(RuntimeHolder(rt));
        cx.set_global(ControlHolder(control));
    });
}

struct RuntimeHolder(#[allow(dead_code)] runtime::Runtime);
impl gpui::Global for RuntimeHolder {}

struct ControlHolder(#[allow(dead_code)] Option<control::ControlServer>);
impl gpui::Global for ControlHolder {}

fn init_tracing() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr))
        .try_init();
}

/// Trace sink that appends one JSON line per event to a file. The
/// `BufWriter` is wrapped in `Mutex` so the cross-thread emit path
/// serializes writes; we flush after every line so a crash never strands
/// in-flight events in the buffer.
struct FileSink {
    writer: Mutex<BufWriter<File>>,
}

impl TraceSink for FileSink {
    fn emit_line(&self, line: &str) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(line.as_bytes());
            let _ = w.write_all(b"\n");
            let _ = w.flush();
        }
    }
}

fn install_trace_file_sink(path: &std::path::Path) {
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(file) => {
            install_sink(Arc::new(FileSink {
                writer: Mutex::new(BufWriter::new(file)),
            }));
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                err = %e,
                "could not open trace file, continuing without it",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use corral_core::trace::{ParsedLine, TraceEvent, clear_sink, emit};
    use tempfile::NamedTempFile;

    #[test]
    fn trace_file_sink_appends_typed_events() {
        let tmp = NamedTempFile::new().expect("tempfile");
        install_trace_file_sink(tmp.path());

        emit(TraceEvent::DiscoveryPassStarted);
        emit(TraceEvent::DiscoveryPassCompleted { agent_count: 0 });
        clear_sink();

        let content = std::fs::read_to_string(tmp.path()).expect("read trace file");
        let parsed: Vec<ParsedLine> = content
            .lines()
            .map(|l| serde_json::from_str(l).expect("trace line is valid JSON"))
            .collect();
        assert_eq!(parsed.len(), 2);
        assert!(matches!(parsed[0].event, TraceEvent::DiscoveryPassStarted));
        assert!(matches!(
            parsed[1].event,
            TraceEvent::DiscoveryPassCompleted { agent_count: 0 }
        ));
    }
}
