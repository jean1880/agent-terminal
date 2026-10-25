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
    use std::os::fd::AsRawFd;
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
    // of buffering a surviving job's output without bound. The drain polls its
    // pipe in 100 ms slices, so it sees `stop` even when the job never writes.
    struct StopOnDrop(Arc<AtomicBool>);
    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    let _stop_on_return = StopOnDrop(Arc::clone(&stop));
    type Drained = (Arc<Mutex<Vec<u8>>>, mpsc::Receiver<()>);
    let drain = |pipe: Option<(Box<dyn Read + Send>, i32)>| -> Drained {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let (done, finished) = mpsc::channel();
        let sink = Arc::clone(&buffer);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            if let Some((mut pipe, fd)) = pipe {
                let mut chunk = [0u8; 8192];
                loop {
                    // Wait for output in short slices so a pipe a descendant
                    // holds open without writing still sees `stop`.
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let mut pfd = libc::pollfd {
                        fd,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // SAFETY: `pfd` is a valid, live pollfd and `fd` stays
                    // open while `pipe` (owned by this thread) is alive.
                    let ready = unsafe { libc::poll(&mut pfd, 1, 100) };
                    if ready == 0 {
                        continue;
                    }
                    if ready < 0 {
                        if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                        {
                            continue;
                        }
                        break;
                    }
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
            .map(|p| (p.as_raw_fd(), Box::new(p) as Box<dyn Read + Send>))
            .map(|(fd, p)| (p, fd)),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| (p.as_raw_fd(), Box::new(p) as Box<dyn Read + Send>))
            .map(|(fd, p)| (p, fd)),
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

    /// A background job outlives the command, holds its stdout open and stays
    /// silent until the test says go (a file created only after `run_command`
    /// returned). The drain must then have let go of the pipe, so the job's
    /// write fails. Nothing depends on how fast the processes run: the job
    /// writes nothing before the signal, and its wait and retries are bounded.
    #[test]
    fn a_silent_background_holder_is_released_after_return() {
        let dir = tempfile::tempdir().unwrap();
        let go = dir.path().join("go");
        let marker = dir.path().join("pipe-closed");
        let script = format!(
            "( trap '' PIPE; i=0; \
               while [ ! -e '{go}' ] && [ $i -lt 500 ]; do sleep 0.02; i=$((i+1)); done; \
               i=0; while [ $i -lt 100 ]; do \
               head -c 1 /dev/zero || {{ touch '{marker}'; exit 0; }}; \
               sleep 0.05; i=$((i+1)); done ) & echo hi",
            go = go.display(),
            marker = marker.display()
        );
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(script);
        let out = run_command(cmd, "sh", 10).unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");

        // Only now may the job write.
        std::fs::write(&go, "").unwrap();
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
