//! Protocol integration companion APIs for spargio runtimes.
//!
//! These helpers provide explicit blocking bridges intended for TLS/WS/QUIC
//! ecosystem integrations that do not natively target spargio executors.

use spargio::{RuntimeError, RuntimeHandle};
use std::io;
use std::time::Duration;

#[derive(Debug, Clone, Copy, Default)]
pub struct BlockingOptions {
    timeout: Option<Duration>,
}

impl BlockingOptions {
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn timeout(self) -> Option<Duration> {
        self.timeout
    }
}

pub async fn tls_blocking<T, F>(handle: &RuntimeHandle, f: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    tls_blocking_with_options(handle, BlockingOptions::default(), f).await
}

pub async fn tls_blocking_with_options<T, F>(
    handle: &RuntimeHandle,
    options: BlockingOptions,
    f: F,
) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    run_blocking(
        handle,
        options,
        f,
        "tls blocking task canceled",
        "tls blocking task timed out",
    )
    .await
}

pub async fn ws_blocking<T, F>(handle: &RuntimeHandle, f: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    ws_blocking_with_options(handle, BlockingOptions::default(), f).await
}

pub async fn ws_blocking_with_options<T, F>(
    handle: &RuntimeHandle,
    options: BlockingOptions,
    f: F,
) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    run_blocking(
        handle,
        options,
        f,
        "ws blocking task canceled",
        "ws blocking task timed out",
    )
    .await
}

pub async fn quic_blocking<T, F>(handle: &RuntimeHandle, f: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    quic_blocking_with_options(handle, BlockingOptions::default(), f).await
}

pub async fn quic_blocking_with_options<T, F>(
    handle: &RuntimeHandle,
    options: BlockingOptions,
    f: F,
) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    run_blocking(
        handle,
        options,
        f,
        "quic blocking task canceled",
        "quic blocking task timed out",
    )
    .await
}

async fn run_blocking<T, F>(
    handle: &RuntimeHandle,
    options: BlockingOptions,
    f: F,
    canceled_msg: &'static str,
    timeout_msg: &'static str,
) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    let join = handle
        .spawn_blocking(f)
        .map_err(runtime_error_to_io_for_blocking)?;
    let joined = match options.timeout() {
        Some(duration) => match spargio::timeout(duration, join).await {
            Ok(result) => result,
            Err(_) => return Err(io::Error::new(io::ErrorKind::TimedOut, timeout_msg)),
        },
        None => join.await,
    };
    joined.map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, canceled_msg))?
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

#[cfg(all(feature = "uring-native", target_os = "linux"))]
pub mod io_compat {
    use futures::io::{AsyncRead, AsyncWrite};
    use spargio::net::TcpStream;
    use std::future::Future;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    type ReadOp = Pin<Box<dyn Future<Output = io::Result<(usize, Vec<u8>)>> + Send + 'static>>;
    type WriteOp = Pin<Box<dyn Future<Output = io::Result<usize>> + Send + 'static>>;

    pub struct FuturesTcpStream {
        inner: TcpStream,
        read_op: Option<ReadOp>,
        write_op: Option<WriteOp>,
    }

    impl FuturesTcpStream {
        pub fn new(inner: TcpStream) -> Self {
            Self {
                inner,
                read_op: None,
                write_op: None,
            }
        }

        pub fn get_ref(&self) -> &TcpStream {
            &self.inner
        }

        pub fn into_inner(self) -> TcpStream {
            self.inner
        }
    }

    impl Unpin for FuturesTcpStream {}

    impl AsyncRead for FuturesTcpStream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            if buf.is_empty() {
                return Poll::Ready(Ok(0));
            }

            if self.read_op.is_none() {
                let inner = self.inner.clone();
                let want = buf.len().max(1);
                self.read_op = Some(Box::pin(
                    async move { inner.recv_owned(vec![0u8; want]).await },
                ));
            }

            match self
                .read_op
                .as_mut()
                .expect("read op set")
                .as_mut()
                .poll(cx)
            {
                Poll::Pending => Poll::Pending,
                Poll::Ready(result) => {
                    self.read_op = None;
                    let (got, payload) = result?;
                    let got = got.min(payload.len()).min(buf.len());
                    buf[..got].copy_from_slice(&payload[..got]);
                    Poll::Ready(Ok(got))
                }
            }
        }
    }

    impl AsyncWrite for FuturesTcpStream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            if buf.is_empty() {
                return Poll::Ready(Ok(0));
            }

            if self.write_op.is_none() {
                let inner = self.inner.clone();
                let payload = buf.to_vec();
                let payload_len = payload.len();
                self.write_op = Some(Box::pin(async move {
                    let (written, _) = inner.send_owned(payload).await?;
                    Ok(written.min(payload_len))
                }));
            }

            match self
                .write_op
                .as_mut()
                .expect("write op set")
                .as_mut()
                .poll(cx)
            {
                Poll::Pending => Poll::Pending,
                Poll::Ready(result) => {
                    self.write_op = None;
                    Poll::Ready(result)
                }
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use std::time::Duration;

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

    #[test]
    fn blocking_timeout_returns_timed_out() {
        let rt = spargio::Runtime::builder()
            .shards(1)
            .build()
            .expect("runtime");
        let err = block_on(async {
            tls_blocking_with_options(
                &rt.handle(),
                BlockingOptions::default().with_timeout(Duration::from_millis(5)),
                || {
                    std::thread::sleep(Duration::from_millis(30));
                    Ok::<(), io::Error>(())
                },
            )
            .await
            .expect_err("timeout")
        });
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }
}
