use futures::executor::block_on;
use spargio_process::{CommandOptions, status_with_options};
use std::io;
use std::process::Command;
use std::time::Duration;

fn slow_command() -> Command {
    if cfg!(windows) {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "ping -n 2 127.0.0.1 > nul"]);
        cmd
    } else {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 0.1"]);
        cmd
    }
}

#[test]
fn status_with_options_enforces_timeout() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");

    let err = block_on(async {
        status_with_options(
            &rt.handle(),
            slow_command(),
            CommandOptions::default().with_timeout(Duration::from_millis(5)),
        )
        .await
        .expect_err("expected timeout")
    });

    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
}
