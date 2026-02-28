//! Process companion APIs for spargio runtimes.
//!
//! These helpers expose async process status/output through spargio's
//! `spawn_blocking` bridge.

use spargio::{RuntimeError, RuntimeHandle};
use std::ffi::OsStr;
use std::io;
use std::process::{Child, Command, ExitStatus, Output};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub async fn status(handle: &RuntimeHandle, command: Command) -> io::Result<ExitStatus> {
    status_with_options(handle, command, CommandOptions::default()).await
}

pub async fn status_with_options(
    handle: &RuntimeHandle,
    mut command: Command,
    options: CommandOptions,
) -> io::Result<ExitStatus> {
    run_blocking(
        handle,
        options,
        move || command.status(),
        "process status task canceled",
        "process status task timed out",
    )
    .await
}

pub async fn output(handle: &RuntimeHandle, command: Command) -> io::Result<Output> {
    output_with_options(handle, command, CommandOptions::default()).await
}

pub async fn output_with_options(
    handle: &RuntimeHandle,
    mut command: Command,
    options: CommandOptions,
) -> io::Result<Output> {
    run_blocking(
        handle,
        options,
        move || command.output(),
        "process output task canceled",
        "process output task timed out",
    )
    .await
}

pub async fn spawn(handle: &RuntimeHandle, command: Command) -> io::Result<ChildHandle> {
    spawn_with_options(handle, command, CommandOptions::default()).await
}

pub async fn spawn_with_options(
    handle: &RuntimeHandle,
    mut command: Command,
    options: CommandOptions,
) -> io::Result<ChildHandle> {
    let child = run_blocking(
        handle,
        options,
        move || command.spawn(),
        "process spawn task canceled",
        "process spawn task timed out",
    )
    .await?;
    Ok(ChildHandle {
        handle: handle.clone(),
        child: Arc::new(Mutex::new(Some(child))),
    })
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CommandOptions {
    timeout: Option<Duration>,
}

impl CommandOptions {
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    fn timeout(self) -> Option<Duration> {
        self.timeout
    }
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

    pub async fn status_with_options(
        self,
        handle: &RuntimeHandle,
        options: CommandOptions,
    ) -> io::Result<ExitStatus> {
        status_with_options(handle, self.command, options).await
    }

    pub async fn output(self, handle: &RuntimeHandle) -> io::Result<Output> {
        output(handle, self.command).await
    }

    pub async fn output_with_options(
        self,
        handle: &RuntimeHandle,
        options: CommandOptions,
    ) -> io::Result<Output> {
        output_with_options(handle, self.command, options).await
    }

    pub async fn spawn(self, handle: &RuntimeHandle) -> io::Result<ChildHandle> {
        spawn(handle, self.command).await
    }

    pub async fn spawn_with_options(
        self,
        handle: &RuntimeHandle,
        options: CommandOptions,
    ) -> io::Result<ChildHandle> {
        spawn_with_options(handle, self.command, options).await
    }
}

#[derive(Clone)]
pub struct ChildHandle {
    handle: RuntimeHandle,
    child: Arc<Mutex<Option<Child>>>,
}

impl ChildHandle {
    pub fn id(&self) -> Option<u32> {
        let guard = self.child.lock().expect("child lock poisoned");
        guard.as_ref().map(Child::id)
    }

    pub async fn wait(&self) -> io::Result<ExitStatus> {
        self.wait_with_options(CommandOptions::default()).await
    }

    pub async fn wait_with_options(&self, options: CommandOptions) -> io::Result<ExitStatus> {
        self.run_with_child(
            options,
            |child| child.wait(),
            "process wait task canceled",
            "process wait task timed out",
        )
        .await
    }

    pub async fn try_wait(&self) -> io::Result<Option<ExitStatus>> {
        self.run_with_child(
            CommandOptions::default(),
            |child| child.try_wait(),
            "process try_wait task canceled",
            "process try_wait task timed out",
        )
        .await
    }

    pub async fn kill(&self) -> io::Result<()> {
        self.run_with_child(
            CommandOptions::default(),
            |child| child.kill(),
            "process kill task canceled",
            "process kill task timed out",
        )
        .await
    }

    pub async fn output(&self) -> io::Result<Output> {
        self.output_with_options(CommandOptions::default()).await
    }

    pub async fn output_with_options(&self, options: CommandOptions) -> io::Result<Output> {
        let child = self.take_child()?;
        let handle = self.handle.clone();
        run_blocking(
            &handle,
            options,
            move || child.wait_with_output(),
            "process output task canceled",
            "process output task timed out",
        )
        .await
    }

    fn take_child(&self) -> io::Result<Child> {
        let mut guard = self.child.lock().expect("child lock poisoned");
        guard
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "child already consumed"))
    }

    async fn run_with_child<T, F>(
        &self,
        options: CommandOptions,
        f: F,
        canceled_msg: &'static str,
        timeout_msg: &'static str,
    ) -> io::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Child) -> io::Result<T> + Send + 'static,
    {
        let child = self.child.clone();
        run_blocking(
            &self.handle,
            options,
            move || {
                let mut guard = child.lock().expect("child lock poisoned");
                let child = guard.as_mut().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "child already consumed")
                })?;
                f(child)
            },
            canceled_msg,
            timeout_msg,
        )
        .await
    }
}

async fn run_blocking<T, F>(
    handle: &RuntimeHandle,
    options: CommandOptions,
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use std::time::Duration;

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

    #[test]
    fn status_with_options_timeout_fails() {
        let rt = spargio::Runtime::builder()
            .shards(1)
            .build()
            .expect("runtime");
        let err = block_on(async {
            status_with_options(
                &rt.handle(),
                if cfg!(windows) {
                    let mut cmd = Command::new("cmd");
                    cmd.args(["/C", "ping -n 2 127.0.0.1 > nul"]);
                    cmd
                } else {
                    let mut cmd = Command::new("sh");
                    cmd.args(["-c", "sleep 0.1"]);
                    cmd
                },
                CommandOptions::default().with_timeout(Duration::from_millis(5)),
            )
            .await
            .expect_err("timeout")
        });
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }
}
