//! Signal companion APIs for spargio runtimes.
//!
//! This crate provides a minimal async-facing signal stream that can be awaited
//! from spargio tasks.

use signal_hook::iterator::Signals;
use std::io;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

pub struct SignalStream {
    rx: Receiver<i32>,
}

impl SignalStream {
    pub async fn recv(&self) -> io::Result<i32> {
        loop {
            match self.rx.try_recv() {
                Ok(sig) => return Ok(sig),
                Err(TryRecvError::Empty) => spargio::sleep(Duration::from_millis(1)).await,
                Err(TryRecvError::Disconnected) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "signal stream closed",
                    ));
                }
            }
        }
    }
}

pub fn signal<I>(signals: I) -> io::Result<SignalStream>
where
    I: IntoIterator<Item = i32>,
{
    let mut signals = Signals::new(signals)?;
    let (tx, rx) = mpsc::channel::<i32>();
    thread::Builder::new()
        .name("spargio-signal-listener".to_owned())
        .spawn(move || {
            for sig in signals.forever() {
                if tx.send(sig).is_err() {
                    break;
                }
            }
        })
        .map_err(|err| io::Error::other(format!("signal thread spawn failed: {err}")))?;

    Ok(SignalStream { rx })
}

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
}
