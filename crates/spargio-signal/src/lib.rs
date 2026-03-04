//! Signal companion APIs for spargio runtimes.
//!
//! This crate provides async-facing signal streams and a small broadcast hub for
//! multi-subscriber shutdown handling.
#![deny(missing_docs)]

use signal_hook::iterator::Signals;
use std::io;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Clone)]
/// Broadcast hub for process signals.
pub struct SignalHub {
    subscribers: Arc<Mutex<Vec<Sender<i32>>>>,
}

impl SignalHub {
    /// Creates a new signal hub for the provided signal numbers.
    ///
    /// A background listener thread is spawned and each received signal is
    /// broadcast to all active subscribers.
    pub fn new<I>(signals: I) -> io::Result<Self>
    where
        I: IntoIterator<Item = i32>,
    {
        let mut signals = Signals::new(signals)?;
        let hub = Self {
            subscribers: Arc::new(Mutex::new(Vec::new())),
        };
        let subscribers = hub.subscribers.clone();

        thread::Builder::new()
            .name("spargio-signal-listener".to_owned())
            .spawn(move || {
                for sig in signals.forever() {
                    let mut subscribers = subscribers.lock().expect("signal hub lock poisoned");
                    subscribers.retain(|tx| tx.send(sig).is_ok());
                }
            })
            .map_err(|err| io::Error::other(format!("signal thread spawn failed: {err}")))?;

        Ok(hub)
    }

    /// Creates a new subscriber stream for this hub.
    pub fn subscribe(&self) -> SignalStream {
        let (tx, rx) = mpsc::channel::<i32>();
        let mut subscribers = self
            .subscribers
            .lock()
            .expect("signal hub subscriber lock poisoned");
        subscribers.push(tx);
        SignalStream {
            rx: Arc::new(Mutex::new(rx)),
        }
    }
}

#[derive(Clone)]
/// Async signal stream view over hub deliveries.
pub struct SignalStream {
    rx: Arc<Mutex<Receiver<i32>>>,
}

impl SignalStream {
    /// Waits for and returns the next signal.
    pub async fn recv(&self) -> io::Result<i32> {
        loop {
            match self.try_recv() {
                Ok(Some(sig)) => return Ok(sig),
                Ok(None) => spargio::sleep(Duration::from_millis(1)).await,
                Err(err) => return Err(err),
            }
        }
    }

    /// Waits for the next signal until `duration` elapses.
    ///
    /// Returns `Ok(None)` on timeout.
    pub async fn recv_timeout(&self, duration: Duration) -> io::Result<Option<i32>> {
        match spargio::timeout(duration, self.recv()).await {
            Ok(result) => result.map(Some),
            Err(_) => Ok(None),
        }
    }

    /// Waits until one of `accepted` signal numbers is received.
    pub async fn recv_matching(&self, accepted: &[i32]) -> io::Result<i32> {
        loop {
            let sig = self.recv().await?;
            if accepted.contains(&sig) {
                return Ok(sig);
            }
        }
    }

    /// Attempts to read one queued signal without waiting.
    pub fn try_recv(&self) -> io::Result<Option<i32>> {
        let rx = self.rx.lock().expect("signal stream lock poisoned");
        match rx.try_recv() {
            Ok(sig) => Ok(Some(sig)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "signal stream closed",
            )),
        }
    }
}

/// Creates a signal stream listening for the specified signal numbers.
pub fn signal<I>(signals: I) -> io::Result<SignalStream>
where
    I: IntoIterator<Item = i32>,
{
    let hub = SignalHub::new(signals)?;
    Ok(hub.subscribe())
}

/// Creates a signal stream for `SIGINT` (Ctrl-C).
pub fn ctrl_c() -> io::Result<SignalStream> {
    signal([signal_hook::consts::SIGINT])
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;

    #[test]
    fn ctrl_c_stream_constructs() {
        let stream = ctrl_c().expect("ctrl_c stream");
        drop(stream);
    }

    #[test]
    fn signal_stream_receives_raised_signal() {
        let stream = signal([signal_hook::consts::SIGUSR1]).expect("signal stream");
        signal_hook::low_level::raise(signal_hook::consts::SIGUSR1).expect("raise SIGUSR1");
        let got = block_on(async {
            spargio::timeout(Duration::from_millis(250), stream.recv())
                .await
                .expect("signal timeout")
                .expect("signal recv")
        });
        assert_eq!(got, signal_hook::consts::SIGUSR1);
    }

    #[test]
    fn hub_broadcasts_to_multiple_streams() {
        let hub = SignalHub::new([signal_hook::consts::SIGUSR2]).expect("hub");
        let a = hub.subscribe();
        let b = hub.subscribe();
        signal_hook::low_level::raise(signal_hook::consts::SIGUSR2).expect("raise");

        block_on(async {
            let got_a = spargio::timeout(Duration::from_millis(250), a.recv())
                .await
                .expect("timeout a")
                .expect("recv a");
            let got_b = spargio::timeout(Duration::from_millis(250), b.recv())
                .await
                .expect("timeout b")
                .expect("recv b");
            assert_eq!(got_a, signal_hook::consts::SIGUSR2);
            assert_eq!(got_b, signal_hook::consts::SIGUSR2);
        });
    }
}
