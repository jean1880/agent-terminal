//! Running a child process with a wall-clock timeout.

use tracing::debug;

/// `run_capture` (in the app's `utils.rs`) for a command already built — for callers that also set
/// its environment. `name` is how errors refer to it.
///
/// Blocking: call it off the main thread.
pub fn run_command(
    mut cmd: std::process::Command,
    name: &str,
    timeout_secs: u64,
) -> Result<std::process::Output, String> {
    use std::io::Read;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::{Duration, Instant};

    let command = name;
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|err| format!("Could not run {command}: {err}"))?;

    // Each pipe is drained on its own thread into a shared buffer, and the
    // thread says when it reached end-of-file. Waiting on that signal with a
    // deadline — never joining the thread — is what keeps the timeout honest:
    // a background job the command started (an rc's `foo &`) inherits the
    // pipes and can hold them open long after the command itself exits.
    // Ceiling: such a job is not killed, only no longer waited for.
    //
    // Once this function returns (by any path) `StopOnDrop` raises `stop`, and
    // a drain that wakes up for more output drops its pipe and exits instead
    // of buffering a surviving job's output without bound.
    struct StopOnDrop(Arc<AtomicBool>);
    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    let _stop_on_return = StopOnDrop(Arc::clone(&stop));
    type Drained = (Arc<Mutex<Vec<u8>>>, mpsc::Receiver<()>);
    let drain = |pipe: Option<Box<dyn Read + Send>>| -> Drained {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let (done, finished) = mpsc::channel();
        let sink = Arc::clone(&buffer);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            if let Some(mut pipe) = pipe {
                let mut chunk = [0u8; 8192];
                loop {
                    match pipe.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(_) if stop.load(Ordering::Relaxed) => break,
                        Ok(n) => {
                            if let Ok(mut bytes) = sink.lock() {
                                bytes.extend_from_slice(&chunk[..n]);
                            }
                        }
                    }
                }
            }
            let _ = done.send(());
        });
        (buffer, finished)
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );

    let deadline = Instant::now() + Duration::from_secs(timeout_secs.max(1));
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{command} did not finish within {timeout_secs}s"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(err) => return Err(format!("Failed while waiting for {command}: {err}")),
        }
    };

    // The command has exited, so everything it wrote is already in the pipes;
    // only a background job writes later. A short grace, shared by both pipes,
    // lets the drains catch up. Waiting until the overall deadline instead
    // would make a leaky rc cost the full timeout on every probe.
    const EXIT_GRACE: Duration = Duration::from_millis(500);
    let grace_end = Instant::now() + EXIT_GRACE;
    let collect = |(buffer, finished): Drained| -> Vec<u8> {
        let wait = grace_end.saturating_duration_since(Instant::now());
        if finished.recv_timeout(wait).is_err() {
            debug!("{command} exited but left its output open; taking what arrived");
        }
        buffer.lock().map(|bytes| bytes.clone()).unwrap_or_default()
    };
    Ok(std::process::Output {
        status,
        stdout: collect(stdout),
        stderr: collect(stderr),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// A background job outlives the command and keeps writing into its
    /// stdout. After `run_command` returns, the drain must let go of the pipe
    /// (the job's next write then fails) instead of buffering the rest forever.
    #[test]
    fn a_leftover_background_writer_is_not_drained_after_return() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("pipe-closed");
        // The writer is bounded by itself: at most 100 x 64 KiB over ~5 s. It
        // ignores SIGPIPE so a closed pipe shows up as a failed write.
        let script = format!(
            "( trap '' PIPE; i=0; while [ $i -lt 100 ]; do \
               head -c 65536 /dev/zero || {{ touch '{}'; exit 0; }}; \
               sleep 0.05; i=$((i+1)); done ) & echo hi",
            marker.display()
        );
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(script);
        let out = run_command(cmd, "sh", 10).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim_start()[..2],
            *"hi"
        );

        let deadline = Instant::now() + Duration::from_secs(8);
        while !marker.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            marker.exists(),
            "the drain kept reading after run_command returned"
        );
    }
}
