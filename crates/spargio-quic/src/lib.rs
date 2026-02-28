//! QUIC companion APIs for spargio runtimes.
//!
//! This crate provides a pragmatic `quinn` bridge: execute quinn async
//! workflows on a Tokio current-thread runtime via spargio's blocking bridge.

use spargio::{RuntimeError, RuntimeHandle};
use std::future::Future;
use std::io;
use std::time::Duration;

pub use quinn;

#[derive(Debug, Clone, Copy, Default)]
pub struct QuicOptions {
    timeout: Option<Duration>,
}

impl QuicOptions {
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn timeout(self) -> Option<Duration> {
        self.timeout
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct QuicBridge {
    options: QuicOptions,
}

impl QuicBridge {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_options(mut self, options: QuicOptions) -> Self {
        self.options = options;
        self
    }

    pub fn options(self) -> QuicOptions {
        self.options
    }

    pub async fn run<T, F, Fut>(&self, handle: &RuntimeHandle, f: F) -> io::Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = io::Result<T>> + Send + 'static,
    {
        run_with_options(handle, self.options, f).await
    }

    pub async fn with_endpoint<T, B, F, Fut>(
        &self,
        handle: &RuntimeHandle,
        build_endpoint: B,
        f: F,
    ) -> io::Result<T>
    where
        T: Send + 'static,
        B: FnOnce() -> io::Result<quinn::Endpoint> + Send + 'static,
        F: FnOnce(quinn::Endpoint) -> Fut + Send + 'static,
        Fut: Future<Output = io::Result<T>> + Send + 'static,
    {
        run_with_options(handle, self.options, move || async move {
            let endpoint = build_endpoint()?;
            f(endpoint).await
        })
        .await
    }
}

pub async fn run<T, F, Fut>(handle: &RuntimeHandle, f: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = io::Result<T>> + Send + 'static,
{
    run_with_options(handle, QuicOptions::default(), f).await
}

pub async fn run_with_options<T, F, Fut>(
    handle: &RuntimeHandle,
    options: QuicOptions,
    f: F,
) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = io::Result<T>> + Send + 'static,
{
    let join = handle
        .spawn_blocking(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|err| io::Error::other(format!("tokio runtime build failed: {err}")))?;
            runtime.block_on(f())
        })
        .map_err(runtime_error_to_io_for_blocking)?;
    let joined = match options.timeout() {
        Some(limit) => match spargio::timeout(limit, join).await {
            Ok(result) => result,
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "quic bridge operation timed out",
                ));
            }
        },
        None => join.await,
    };
    joined.map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "quic bridge task canceled"))?
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
