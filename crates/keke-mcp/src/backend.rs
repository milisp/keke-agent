use std::sync::OnceLock;

use tokio::runtime::Handle;

/// A runtime that outlives any one session, used for every MCP child process.
///
/// Two constraints force this. `ToolContributor::tools` is synchronous, but
/// learning a server's tool list means asking the server, which is I/O; and a
/// connection opened while listing has to keep driving its reader task for the
/// rest of the session, long after the listing call returned. Both are solved
/// by owning a runtime on a thread of our own: the caller hands work over and
/// waits on a plain channel, so nothing re-enters the session's runtime and
/// nothing can deadlock against it.
///
/// `None` means the thread or its runtime could not be created, which every
/// caller reports as a failure of the servers rather than as a panic.
pub(crate) fn backend() -> Option<&'static Handle> {
    static BACKEND: OnceLock<Option<Handle>> = OnceLock::new();
    BACKEND
        .get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .name("keke-mcp".to_string())
                .spawn(move || {
                    let runtime = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(runtime) => runtime,
                        Err(error) => {
                            tracing::error!(%error, "no runtime for MCP servers");
                            return;
                        }
                    };
                    if tx.send(runtime.handle().clone()).is_ok() {
                        // Parked forever: the handle is what callers use, and the
                        // runtime must stay driven for as long as they hold it.
                        runtime.block_on(std::future::pending::<()>());
                    }
                })
                .ok();
            rx.recv().ok()
        })
        .as_ref()
}
