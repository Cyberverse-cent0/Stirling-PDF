//! Per-request progress emission. Port of `stirling.services.progress`: the
//! Python emitter is an async callback on a contextvar; here it is an
//! unbounded channel sender on a task-local, so `emit_progress` stays
//! synchronous and fire-and-forget. Outside a scoped request nothing is
//! bound and emission is a no-op.

use std::future::Future;

use tokio::sync::mpsc::UnboundedSender;

use crate::contracts::ProgressEvent;

tokio::task_local! {
    static PROGRESS_TX: UnboundedSender<ProgressEvent>;
}

/// Run `fut` with `tx` as the progress sink for this task tree. The receiver
/// side is owned by the streaming route, which serialises events to NDJSON.
pub async fn scoped<F: Future>(tx: UnboundedSender<ProgressEvent>, fut: F) -> F::Output {
    PROGRESS_TX.scope(tx, fut).await
}

/// Publish `event` to the current request's sink, if any. A closed channel
/// (client disconnected) drops the event silently — progress must never
/// break the work it reports on.
pub fn emit_progress(event: ProgressEvent) {
    let _ = PROGRESS_TX.try_with(|tx| tx.send(event));
}
