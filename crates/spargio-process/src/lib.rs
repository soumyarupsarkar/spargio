//! Process companion APIs for spargio runtimes.
//!
//! These helpers expose async process status/output through spargio's
//! `spawn_blocking` bridge.

use spargio::{RuntimeError, RuntimeHandle};
use std::ffi::OsStr;
use std::io;
use std::process::{Command, ExitStatus, Output};

pub async fn status(handle: &RuntimeHandle, mut command: Command) -> io::Result<ExitStatus> {
    run_blocking(
        handle,
        move || command.status(),
        "process status task canceled",
    )
    .await
}

pub async fn output(handle: &RuntimeHandle, mut command: Command) -> io::Result<Output> {
    run_blocking(
        handle,
        move || command.output(),
        "process output task canceled",
    )
    .await
}

pub struct CommandBuilder {
    command: Command,
}

impl CommandBuilder {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            command: Command::new(program),
        }
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.command.arg(arg);
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.command.args(args);
        self
    }

    pub async fn status(self, handle: &RuntimeHandle) -> io::Result<ExitStatus> {
        status(handle, self.command).await
    }

    pub async fn output(self, handle: &RuntimeHandle) -> io::Result<Output> {
        output(handle, self.command).await
    }
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

    fn success_command() -> Command {
        if cfg!(windows) {
            let mut cmd = Command::new("cmd");
            cmd.args(["/C", "exit", "0"]);
            cmd
        } else {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", "exit 0"]);
            cmd
        }
    }

    #[test]
    fn command_builder_status_runs() {
        let rt = spargio::Runtime::builder()
            .shards(1)
            .build()
            .expect("runtime");
        let status = block_on(async {
            CommandBuilder::new(if cfg!(windows) { "cmd" } else { "sh" })
                .args(if cfg!(windows) {
                    vec!["/C", "exit", "0"]
                } else {
                    vec!["-c", "exit 0"]
                })
                .status(&rt.handle())
                .await
                .expect("status")
        });
        assert!(status.success());
    }

    #[test]
    fn status_function_runs() {
        let rt = spargio::Runtime::builder()
            .shards(1)
            .build()
            .expect("runtime");
        let status = block_on(async {
            status(&rt.handle(), success_command())
                .await
                .expect("status")
        });
        assert!(status.success());
    }
}
