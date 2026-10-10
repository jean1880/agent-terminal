//! `agent-terminal --approval-hook`: the client half of agy's PreToolUse gate.
//!
//! A short-lived process, run before any GTK initialisation, so plain blocking std I/O is right.
//! agy writes its payload to our stdin; we forward it to the app over the unix socket named by
//! `AGENT_TERMINAL_APPROVAL_SOCKET` and print what the app decides. Every failure prints the deny
//! output: fail closed. With the variable unset (agy started by something else) we print nothing
//! and exit 0, which agy reads as "allow", so those sessions are untouched.
//!
//! Timeouts: the hook entry is installed with `timeout` 600 s. The server answers deny at 570 s
//! ([`crate::approval_server::DEFAULT_DEADLINE`]); this client gives up at [`REPLY_TIMEOUT`]
//! (590 s), a backstop for a server that died without closing the connection.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use agent_core::approval::{self, HookInput};

/// How long the hook waits for the app's reply: just under the hook's configured 600 s.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(590);
/// Longest reply line accepted.
const MAX_REPLY: usize = 64 * 1024;

/// The hook's whole behaviour. Returns the text for stdout (empty means "print nothing").
pub fn run_hook(stdin: &str, env_socket: Option<&str>, timeout: Duration) -> String {
    if !approval::should_gate(env_socket) {
        return String::new();
    }
    let reply = env_socket.and_then(|socket| ask(stdin, socket.trim(), timeout));
    approval::hook_output(reply.as_ref())
}

fn ask(stdin: &str, socket: &str, timeout: Duration) -> Option<approval::ApprovalReply> {
    let input = HookInput::parse(stdin).ok()?;
    let id = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    );
    let query = approval::encode_query(&input.to_query(id)).ok()?;
    let deadline = Instant::now() + timeout;

    let mut stream = UnixStream::connect(socket).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    stream.write_all(query.as_bytes()).ok()?;
    stream.write_all(b"\n").ok()?;

    // One line back. The read timeout is re-armed with what is left of the overall deadline, so
    // a trickling server cannot stretch it.
    let mut line = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        stream
            .set_read_timeout(Some(left.max(Duration::from_millis(1))))
            .ok()?;
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        line.extend_from_slice(&chunk[..n]);
        if line.contains(&b'\n') || line.len() > MAX_REPLY {
            break;
        }
    }
    let text = String::from_utf8(line).ok()?;
    let first = text.lines().next()?;
    approval::parse_reply(first).ok()
}

unsafe extern "C" fn handle_signal(_: libc::c_int) {
    let msg = b"{\"decision\":\"deny\",\"reason\":\"hook process terminated\"}\n";
    let _ = libc::write(libc::STDOUT_FILENO, msg.as_ptr().cast(), msg.len());
    libc::_exit(0);
}

/// Entry point for `--approval-hook`: reads stdin, prints the decision, exits 0.
pub fn main() -> std::process::ExitCode {
    unsafe {
        libc::signal(libc::SIGTERM, handle_signal as *const () as usize);
        libc::signal(libc::SIGINT, handle_signal as *const () as usize);
        libc::signal(libc::SIGHUP, handle_signal as *const () as usize);
    }
    let mut stdin = String::new();
    if std::io::stdin().read_to_string(&mut stdin).is_err() {
        stdin.clear(); // unreadable payload: a gated call then denies
    }
    let env = std::env::var(approval::ENV_SOCKET).ok();
    let out = run_hook(&stdin, env.as_deref(), REPLY_TIMEOUT);
    if !out.is_empty() {
        let mut stdout = std::io::stdout();
        let _ = writeln!(stdout, "{out}");
        let _ = stdout.flush();
    }
    std::process::ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::approval::{ApprovalQuery, ApprovalReply};
    use agent_core::event::Decision;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    use std::thread;

    const PAYLOAD: &str = r#"{"toolCall":{"name":"run_command","args":{"CommandLine":"echo hi"}},"conversationId":"c1","workspacePaths":["/work/repo"]}"#;
    const T: Duration = Duration::from_secs(5);

    fn decision(out: &str) -> String {
        let v: serde_json::Value = serde_json::from_str(out).expect("json output");
        v["decision"].as_str().expect("decision").to_owned()
    }

    #[test]
    fn unset_or_blank_socket_prints_nothing() {
        assert_eq!(run_hook(PAYLOAD, None, T), "");
        assert_eq!(run_hook(PAYLOAD, Some(""), T), "");
        assert_eq!(run_hook(PAYLOAD, Some("   "), T), "");
        assert_eq!(run_hook("garbage", None, T), "");
    }

    #[test]
    fn a_dead_socket_denies() {
        let tmp = tempfile::tempdir().expect("tmp");
        let missing = tmp.path().join("nope.sock");
        let out = run_hook(PAYLOAD, Some(&missing.to_string_lossy()), T);
        assert_eq!(decision(&out), "deny");
        // A socket file nobody listens on (stale) also denies.
        let stale = tmp.path().join("stale.sock");
        drop(UnixListener::bind(&stale).expect("bind"));
        let out = run_hook(PAYLOAD, Some(&stale.to_string_lossy()), T);
        assert_eq!(decision(&out), "deny");
    }

    #[test]
    fn malformed_stdin_denies_without_touching_the_socket() {
        let tmp = tempfile::tempdir().expect("tmp");
        let sock = tmp.path().join("s.sock");
        let listener = UnixListener::bind(&sock).expect("bind");
        listener.set_nonblocking(true).expect("nb");
        for bad in ["", "not json", r#"{"conversationId":"c"}"#] {
            let out = run_hook(bad, Some(&sock.to_string_lossy()), T);
            assert_eq!(decision(&out), "deny", "{bad}");
        }
        assert!(listener.accept().is_err(), "nothing connected");
    }

    fn serve_once(
        reply: Option<String>,
    ) -> (tempfile::TempDir, String, thread::JoinHandle<String>) {
        let tmp = tempfile::tempdir().expect("tmp");
        let sock = tmp.path().join("s.sock");
        let listener = UnixListener::bind(&sock).expect("bind");
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).expect("read");
            if let Some(reply) = reply {
                (&stream).write_all(reply.as_bytes()).expect("reply");
            }
            line
        });
        (tmp, sock.to_string_lossy().into_owned(), handle)
    }

    #[test]
    fn allow_and_deny_replies_are_relayed() {
        let allow = approval::encode_reply(&ApprovalReply {
            decision: Decision::Allow,
            reason: None,
        })
        .expect("encode");
        let (_t, sock, server) = serve_once(Some(format!("{allow}\n")));
        let out = run_hook(PAYLOAD, Some(&sock), T);
        assert_eq!(decision(&out), "allow");
        let query: ApprovalQuery =
            approval::parse_query(&server.join().expect("server")).expect("query");
        assert_eq!(query.tool, "run_command");
        assert_eq!(query.args["CommandLine"], "echo hi");
        assert_eq!(query.cwd.as_deref(), Some("/work/repo"));

        let deny = approval::encode_reply(&ApprovalReply {
            decision: Decision::Deny,
            reason: Some("no thanks".into()),
        })
        .expect("encode");
        let (_t, sock, server) = serve_once(Some(format!("{deny}\n")));
        let out = run_hook(PAYLOAD, Some(&sock), T);
        assert_eq!(decision(&out), "deny");
        assert!(out.contains("no thanks"));
        server.join().expect("server");
    }

    #[test]
    fn garbage_eof_and_silence_all_deny() {
        let (_t, sock, server) = serve_once(Some("garbage\n".into()));
        assert_eq!(decision(&run_hook(PAYLOAD, Some(&sock), T)), "deny");
        server.join().expect("server");

        // The server closes without answering.
        let (_t, sock, server) = serve_once(None);
        assert_eq!(decision(&run_hook(PAYLOAD, Some(&sock), T)), "deny");
        server.join().expect("server");

        // The server never answers: the client's own deadline fires.
        let tmp = tempfile::tempdir().expect("tmp");
        let sock = tmp.path().join("s.sock");
        let listener = UnixListener::bind(&sock).expect("bind");
        let hold = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            thread::sleep(Duration::from_millis(1500));
            drop(stream);
        });
        let started = Instant::now();
        let out = run_hook(
            PAYLOAD,
            Some(&sock.to_string_lossy()),
            Duration::from_millis(300),
        );
        assert_eq!(decision(&out), "deny");
        assert!(started.elapsed() < Duration::from_millis(1200));
        hold.join().expect("hold");
    }

    #[test]
    fn the_timeout_sits_under_the_hooks_600_seconds_and_over_the_servers_deadline() {
        assert!(REPLY_TIMEOUT < Duration::from_secs(600));
        assert!(REPLY_TIMEOUT > crate::approval_server::DEFAULT_DEADLINE);
    }
}
