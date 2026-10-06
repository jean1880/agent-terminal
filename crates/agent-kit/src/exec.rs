//! Running a child process with a wall-clock timeout.

use tracing::debug;

/// [`run_capture`] for a command already built — for callers that also set
/// its environment. `name` is how errors refer to it.
///
/// Blocking: call it off the main thread.
pub fn run_command(
    mut cmd: std::process::Command,
    name: &str,
    timeout_secs: u64,
) -> Result<std::process::Output, String> {
    use std::io::Read;
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
    type Drained = (Arc<Mutex<Vec<u8>>>, mpsc::Receiver<()>);
    let drain = |pipe: Option<Box<dyn Read + Send>>| -> Drained {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let (done, finished) = mpsc::channel();
        let sink = Arc::clone(&buffer);
        std::thread::spawn(move || {
            if let Some(mut pipe) = pipe {
                let mut chunk = [0u8; 8192];
                loop {
                    match pipe.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
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
