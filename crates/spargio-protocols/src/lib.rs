//! Protocol integration companion APIs for spargio runtimes.
//!
//! These helpers provide explicit blocking bridges intended for TLS/WS/QUIC
//! ecosystem integrations that do not natively target spargio executors.

use spargio::{RuntimeError, RuntimeHandle};
use std::io;

pub async fn tls_blocking<T, F>(handle: &RuntimeHandle, f: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    run_blocking(handle, f, "tls blocking task canceled").await
}

pub async fn ws_blocking<T, F>(handle: &RuntimeHandle, f: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    run_blocking(handle, f, "ws blocking task canceled").await
}

pub async fn quic_blocking<T, F>(handle: &RuntimeHandle, f: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    run_blocking(handle, f, "quic blocking task canceled").await
}

async fn run_blocking<T, F>(
    handle: &RuntimeHandle,
    f: F,
    canceled_msg: &'static str,
) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    let join = handle
        .spawn_blocking(f)
        .map_err(runtime_error_to_io_for_blocking)?;
    join.await
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, canceled_msg))?
}

fn runtime_error_to_io_for_blocking(err: RuntimeError) -> io::Error {
    match err {
        RuntimeError::InvalidConfig(msg) => io::Error::new(io::ErrorKind::InvalidInput, msg),
        RuntimeError::ThreadSpawn(io) => io,
        RuntimeError::InvalidShard(shard) => {
            io::Error::new(io::ErrorKind::NotFound, format!("invalid shard {shard}"))
        }
        RuntimeError::Closed => io::Error::new(io::ErrorKind::BrokenPipe, "runtime closed"),
        RuntimeError::Overloaded => io::Error::new(io::ErrorKind::WouldBlock, "runtime overloaded"),
        RuntimeError::UnsupportedBackend(msg) => io::Error::new(io::ErrorKind::Unsupported, msg),
        RuntimeError::IoUringInit(io) => io,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;

    #[test]
    fn protocol_blocking_helpers_execute_closure() {
        let rt = spargio::Runtime::builder()
            .shards(1)
            .build()
            .expect("runtime");
        let handle = rt.handle();

        let tls = block_on(async { tls_blocking(&handle, || Ok::<_, io::Error>(11usize)).await })
            .expect("tls");
        let ws = block_on(async { ws_blocking(&handle, || Ok::<_, io::Error>(22usize)).await })
            .expect("ws");
        let quic = block_on(async { quic_blocking(&handle, || Ok::<_, io::Error>(33usize)).await })
            .expect("quic");

        assert_eq!(tls + ws + quic, 66);
    }
}
