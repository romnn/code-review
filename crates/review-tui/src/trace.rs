//! A Perfetto trace of the app's spans, written with `--trace-output <FILE>` by a build with the
//! `trace` feature, for profiling: open it in the Perfetto UI or query it with trace processor.
//!
//! Only the spans of the app's own crates are recorded, and the subscriber is the process's
//! global one, since jobs open their spans on threads of their own.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::Mutex;

use color_eyre::eyre::{self, WrapErr as _};
use tracing_perfetto::{FinishHandle, PerfettoLayer};
use tracing_subscriber::Layer as _;
use tracing_subscriber::layer::SubscriberExt as _;

/// The trace being written; [`Recording::finish`] completes it.
pub struct Recording {
    finish: FinishHandle,
}

/// Starts writing the trace to `path`, replacing the file.
///
/// # Errors
///
/// Returns an error when the file cannot be created or a global subscriber is already set.
pub fn record(path: &Path) -> eyre::Result<Recording> {
    let file = File::create(path)
        .wrap_err_with(|| format!("cannot create the trace {}", path.display()))?;
    let layer = PerfettoLayer::new(Mutex::new(BufWriter::new(file))).with_debug_annotations(true);
    let finish = layer.finish_handle();
    let layer = layer.with_filter(tracing_subscriber::filter::filter_fn(|metadata| {
        metadata.target().starts_with("review_")
    }));
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer))
        .wrap_err("cannot install the trace's subscriber")?;
    Ok(Recording { finish })
}

impl Recording {
    /// Ends the trace and flushes it to its file.
    ///
    /// # Errors
    ///
    /// Returns the first error writing or flushing the trace.
    pub fn finish(self) -> eyre::Result<()> {
        self.finish
            .finish()
            .wrap_err("cannot write the trace's end")
    }
}
