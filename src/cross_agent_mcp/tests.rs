use std::time::{Duration, Instant};

use agent_core::adapter::Driver;
use agent_core::delegation::ipc::{
    Call, CallOutcome, CallResult, Frame, IpcLimits, Principal, Welcome, MAX_FRAME,
};
use agent_core::delegation::{ErrorCode, ToolName, WorkerId};
use rmcp::service::RoleServer;
use rmcp::transport::Transport;
use rmcp::ServiceExt;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use super::client::{self, test_support};
use super::stdio::{bounded_ids, bounded_wire_ids, BoundedTransport};
use super::{Bridge, Client};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("runtime")
}

fn welcome() -> Frame {
    Frame::Welcome(Welcome {
        api: 1,
        connection: "cn_test".into(),
        principal: Principal {
            worker_id: WorkerId::parse("w_abcdefgh").expect("worker"),
            root_id: WorkerId::parse("w_abcdefgh").expect("root"),
            driver: Driver::Claude,
            depth: 0,
            can_spawn: true,
            task_mode_ceiling: Vec::new(),
        },
        authority_epoch: 1,
        limits: IpcLimits {
            max_frame: MAX_FRAME as u32,
            max_inflight: 4,
        },
    })
}

async fn accept(listener: &UnixListener) -> UnixStream {
    let (mut socket, _) = listener.accept().await.expect("accept");
    assert!(matches!(
        test_support::read(&mut socket).await,
        Frame::Hello(_)
    ));
    test_support::write(&mut socket, &welcome()).await;
    socket
}

/// A socket for the app's side of the IPC and a client started at `started` that dials it.
/// Nothing is answered: each test accepts (and says hello back) itself. The folder holding the
/// socket lives as long as the returned guard.
fn listening_client(started: Instant) -> (tempfile::TempDir, UnixListener, Client) {
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("ipc.sock");
    let listener = UnixListener::bind(&path).expect("bind");
    let client = Client::start(Ok(test_support::config(path)), started);
    (temp, listener, client)
}

type PeerReader = BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>;
type PeerWriter = tokio::io::WriteHalf<tokio::io::DuplexStream>;

/// Serves `bridge` over an in-memory pipe and returns the serving task and the MCP client's
/// ends of the pipe. Nothing is sent: each test drives the lifecycle itself.
fn serve_bridge(bridge: Bridge) -> (tokio::task::JoinHandle<impl Sized>, PeerReader, PeerWriter) {
    let (server, peer) = tokio::io::duplex(MAX_FRAME);
    let (read, write) = tokio::io::split(server);
    let transport =
        BoundedTransport::with_io(read, write).with_cancellations(bridge.cancellations.clone());
    let serving = tokio::spawn(async move {
        bridge
            .serve(transport)
            .await
            .expect("serve")
            .waiting()
            .await
    });
    let (read, write) = tokio::io::split(peer);
    (serving, BufReader::new(read), write)
}

fn call(id: u64) -> Call {
    Call {
        id,
        tool: ToolName::CrossAgentCapabilities,
        args: json!({}),
        timeout_ms: 2_000,
        invocation: None,
    }
}

#[test]
fn lineage_stat_uses_the_last_parenthesis() {
    assert_eq!(
        client::parse_ppid("42 (a tricky ) process (name)) S 123 0 0"),
        Some(123)
    );
    assert_eq!(client::parse_ppid("42 (x) S nope"), None);
    assert_eq!(client::parse_ppid("garbage"), None);
}

#[test]
fn ids_are_bounded_in_the_original_encoding_including_cancellation() {
    for (wire, valid) in [
        (format!("\"{}\"", "x".repeat(254)), true),
        (format!("\"{}\"", "x".repeat(255)), false),
        (format!("\"{}\"", "\\u0061".repeat(42)), true),
        (format!("\"{}\"", "\\u0061".repeat(43)), false),
        ("1".repeat(256), true),
        ("1".repeat(257), false),
    ] {
        let request = format!("{{\"jsonrpc\":\"2.0\",\"method\":\"ping\",\"id\":{wire}}}");
        assert_eq!(bounded_wire_ids(request.as_bytes()), valid, "{wire}");
        let cancelled = format!(
            "{{\"method\":\"notifications/cancelled\",\"params\":{{\"requestId\":{wire}}}}}"
        );
        assert_eq!(bounded_wire_ids(cancelled.as_bytes()), valid);
    }
    assert!(bounded_ids(&json!({"id": "x".repeat(254)})));
    assert!(!bounded_ids(&json!({"id": "x".repeat(255)})));
}

#[test]
fn a_line_over_one_mib_closes_before_json_parsing() {
    runtime().block_on(async {
        let input = vec![b'x'; MAX_FRAME + 1];
        let mut transport = BoundedTransport::with_io(input.as_slice(), tokio::io::sink());
        assert!(Transport::<RoleServer>::receive(&mut transport)
            .await
            .is_none());
    });
}

#[test]
fn oversized_ids_reply_with_null_and_do_not_reach_sdk() {
    runtime().block_on(async {
        let input = format!("{{\"jsonrpc\":\"2.0\",\"id\":\"{}\",\"method\":\"ping\"}}\n{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"ping\"}}\n", "x".repeat(255));
        let (output, peer) = tokio::io::duplex(4096);
        let mut transport = BoundedTransport::with_io(input.as_bytes(), output);
        let accepted = Transport::<RoleServer>::receive(&mut transport).await.expect("next request");
        assert_eq!(serde_json::to_value(accepted).expect("json")["id"], 3);
        let mut peer = BufReader::new(peer);
        let mut line = String::new();
        peer.read_line(&mut line).await.expect("reply");
        let reply: Value = serde_json::from_str(&line).expect("json");
        assert_eq!(reply["id"], Value::Null);
        assert_eq!(reply["error"]["code"], -32600);
        assert!(!line.contains(&"x".repeat(255)));
    });
}

#[test]
fn hello_is_eager_and_calls_multiplex_without_reordering_results() {
    runtime().block_on(async {
        let (_temp, listener, client) = listening_client(Instant::now());
        // No tools/list or call has happened: hello must already arrive.
        let mut socket = tokio::time::timeout(Duration::from_millis(500), accept(&listener))
            .await
            .expect("eager hello");
        client.ready().await.expect("ready");
        let first_client = client.clone();
        let first = tokio::spawn(async move { first_client.call(call(1)).await });
        let second_client = client.clone();
        let second = tokio::spawn(async move { second_client.call(call(2)).await });
        let a = test_support::read(&mut socket).await;
        let b = test_support::read(&mut socket).await;
        assert!(matches!(a, Frame::Call(_)) && matches!(b, Frame::Call(_)));
        for id in [2, 1] {
            test_support::write(
                &mut socket,
                &Frame::Result(CallResult {
                    id,
                    outcome: CallOutcome::Ok(json!({"id":id})),
                }),
            )
            .await;
        }
        assert_eq!(
            first.await.expect("join").expect("call"),
            CallOutcome::Ok(json!({"id":1}))
        );
        assert_eq!(
            second.await.expect("join").expect("call"),
            CallOutcome::Ok(json!({"id":2}))
        );
    });
}

#[test]
fn cancelling_an_mcp_handler_releases_its_ipc_wait() {
    runtime().block_on(async {
        let (_temp, listener, client) = listening_client(Instant::now());
        let mut socket = accept(&listener).await;
        let running = tokio::spawn(async move { client.call(call(7)).await });
        assert!(matches!(
            test_support::read(&mut socket).await,
            Frame::Call(Call { id: 7, .. })
        ));
        running.abort();
        let _ = running.await;
        assert_eq!(
            test_support::read(&mut socket).await,
            Frame::Cancel { id: 7 }
        );
    });
}

#[test]
fn losing_a_call_never_replays_it_and_reconnects_only_once() {
    runtime().block_on(async {
        let (_temp, listener, client) = listening_client(Instant::now());
        let mut socket = accept(&listener).await;
        let caller = client.clone();
        let running = tokio::spawn(async move { caller.call(call(1)).await });
        assert!(matches!(
            test_support::read(&mut socket).await,
            Frame::Call(_)
        ));
        drop(socket);
        assert_eq!(
            running.await.expect("join").expect_err("lost").code,
            ErrorCode::TransportLost
        );
        let mut reconnected = accept(&listener).await;
        client.ready().await.expect("reconnected");
        // The mutation was not resent on reconnect.
        assert!(tokio::time::timeout(
            Duration::from_millis(50),
            test_support::read(&mut reconnected)
        )
        .await
        .is_err());
        drop(reconnected);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            client.ready().await.expect_err("degraded").code,
            ErrorCode::RuntimeUnavailable
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    });
}

/// agy's capability is spent by its first hello: a lost connection degrades the bridge at once
/// and never presents that used credential again.
#[test]
fn an_agy_bridge_never_reconnects_with_its_spent_capability() {
    runtime().block_on(async {
        let (_temp, listener, client) = listening_client(Instant::now());
        let (mut socket, _) = listener.accept().await.expect("accept");
        assert!(matches!(
            test_support::read(&mut socket).await,
            Frame::Hello(_)
        ));
        let Frame::Welcome(mut agy) = welcome() else {
            unreachable!()
        };
        agy.principal.driver = Driver::Agy;
        test_support::write(&mut socket, &Frame::Welcome(agy)).await;
        client.ready().await.expect("ready");
        drop(socket);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            client.ready().await.expect_err("degraded").code,
            ErrorCode::RuntimeUnavailable
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "the spent capability was presented again"
        );
    });
}

#[test]
fn welcome_deadline_starts_at_process_start() {
    runtime().block_on(async {
        let (_temp, listener, client) = listening_client(Instant::now() - Duration::from_secs(3));
        let _accept = tokio::spawn(async move {
            let _ = listener.accept().await;
            tokio::time::sleep(Duration::from_secs(3)).await;
        });
        let error = tokio::time::timeout(Duration::from_millis(100), client.ready())
            .await
            .expect("already expired")
            .expect_err("degraded");
        assert_eq!(error.code, ErrorCode::RuntimeUnavailable);
    });
}

async fn read_json(reader: &mut (impl tokio::io::AsyncBufRead + Unpin)) -> Value {
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(3), reader.read_line(&mut line))
        .await
        .expect("deadline")
        .expect("read");
    serde_json::from_str(&line).expect("JSON reply")
}

#[test]
fn rmcp_lifecycle_and_degraded_tool_errors_are_structured() {
    runtime().block_on(async {
        let bridge = Bridge::new(Client::start(Err(super::unavailable("not launched by the app")), Instant::now()));
        let (serving, mut reader, mut writer) = serve_bridge(bridge);
        writer.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{},\"clientInfo\":{\"name\":\"test\",\"version\":\"1\"}}}\n").await.expect("init");
        let initialised = read_json(&mut reader).await;
        assert_eq!(initialised["result"]["serverInfo"]["name"], "agent-terminal");
        writer.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n").await.expect("list");
        let listed = read_json(&mut reader).await;
        assert_eq!(listed["result"]["tools"].as_array().expect("tools").len(), 1);
        assert_eq!(listed["result"]["tools"][0]["name"], "cross_agent_capabilities");
        writer.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"cross_agent_capabilities\",\"arguments\":{}}}\n").await.expect("call");
        let failed = read_json(&mut reader).await;
        assert_eq!(failed["result"]["isError"], true);
        assert_eq!(failed["result"]["structuredContent"]["error"]["code"], "runtime_unavailable");
        assert_eq!(serde_json::from_str::<Value>(failed["result"]["content"][0]["text"].as_str().expect("text")).expect("json"), failed["result"]["structuredContent"]);
        writer.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"no_such_tool\",\"arguments\":{}}}\n").await.expect("unknown");
        assert_eq!(read_json(&mut reader).await["error"]["code"], -32602);
        writer.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"tools/call\",\"params\":{\"name\":\"cross_agent_capabilities\",\"arguments\":{\"profile_id\":23}}}\n").await.expect("invalid");
        assert_eq!(read_json(&mut reader).await["result"]["structuredContent"]["error"]["code"], "invalid_argument");
        writer.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":6,\"method\":\"not/a/method\"}\n").await.expect("unknown method");
        assert_eq!(read_json(&mut reader).await["error"]["code"], -32601);
        writer.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/call\",\"params\":{\"name\":\"cross_agent_capabilities\",\"arguments\":[]}}\n").await.expect("non-object args");
        assert_eq!(read_json(&mut reader).await["result"]["structuredContent"]["error"]["code"], "invalid_argument");
        serving.abort();
    });
}

async fn write_json(writer: &mut (impl tokio::io::AsyncWrite + Unpin), value: Value) {
    let mut bytes = serde_json::to_vec(&value).expect("encode");
    bytes.push(b'\n');
    writer.write_all(&bytes).await.expect("write");
}

#[test]
fn sdk_rejections_before_handler_registration_do_not_leak_cancellations() {
    runtime().block_on(async {
        let bridge = Bridge::new(Client::start(Err(super::unavailable("unbound")), Instant::now()));
        let cancellations = bridge.cancellations.clone();
        let (serving, mut read, mut write) = serve_bridge(bridge);
        let meta = json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28", "io.modelcontextprotocol/clientCapabilities":{}, "io.modelcontextprotocol/clientInfo":{"name":"test","version":"1"}});
        write_json(&mut write, json!({"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":meta}})).await;
        assert!(read_json(&mut read).await.get("result").is_some());
        for id in 2..130 {
            // Valid typed input reaches the SDK; missing mandatory 2026 metadata is rejected
            // there, before Bridge::call_tool can create its Registration drop guard.
            write_json(&mut write, json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"cross_agent_capabilities","arguments":{}}})).await;
            let reply = read_json(&mut read).await;
            assert_eq!(reply["id"], id);
            assert!(reply.get("error").is_some(), "SDK must reject metadata before handler");
            assert_eq!(cancellations.active_count(), 0, "SDK rejection leaked request {id}");
        }
        serving.abort();
    });
}

#[test]
fn cancelled_request_guard_cannot_remove_a_later_reused_id() {
    runtime().block_on(async {
        let cancellations = super::stdio::CancellationRegistry::default();
        let (server, peer) = tokio::io::duplex(MAX_FRAME);
        let (read, write) = tokio::io::split(server);
        let mut transport = BoundedTransport::with_io(read, write).with_cancellations(cancellations.clone());
        let (read, mut write) = tokio::io::split(peer);
        let mut read = BufReader::new(read);
        let call = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"cross_agent_capabilities","arguments":{}}});
        write_json(&mut write, call.clone()).await;
        assert!(transport.receive().await.is_some());
        write_json(&mut write, json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}})).await;
        assert!(transport.receive().await.is_some());
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = cancelled.clone();
        let old = cancellations.register("1".into(), move || { seen.store(true, std::sync::atomic::Ordering::Relaxed); });
        assert!(cancelled.load(std::sync::atomic::Ordering::Relaxed), "pre-handler cancellation is retained");
        let reply = serde_json::from_value(json!({"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"cancelled"}})).expect("typed reply");
        transport.send(reply).await.expect("reply");
        let _ = read_json(&mut read).await;
        assert_eq!(cancellations.active_count(), 0);
        write_json(&mut write, call).await;
        assert!(transport.receive().await.is_some());
        let new = cancellations.register("1".into(), || {});
        drop(old);
        assert_eq!(cancellations.active_count(), 1, "late old guard removed reused id");
        drop(new);
        assert_eq!(cancellations.active_count(), 0);
    });
}

#[test]
fn discover_metadata_and_cancellation_work_without_initialize() {
    runtime().block_on(async {
        let (_temp, listener, client) = listening_client(Instant::now());
        let mut socket = accept(&listener).await;
        let (serving, mut read, mut write) = serve_bridge(Bridge::new(client));
        let meta = json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28", "io.modelcontextprotocol/clientCapabilities":{}, "io.modelcontextprotocol/clientInfo":{"name":"test","version":"1"}});
        write_json(&mut write, json!({"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":meta}})).await;
        let discovered = read_json(&mut read).await;
        assert_eq!(discovered["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"], "agent-terminal");
        assert!(discovered["result"]["supportedVersions"].as_array().expect("versions").iter().any(|version| version == "2026-07-28"));
        let mut invocation_meta = meta.clone();
        invocation_meta["claudecode/toolUseId"] = json!("toolu_provider_confirmed");
        invocation_meta["x-codex-turn-metadata"] = json!({"threadId":"untrusted","callId":"untrusted"});
        write_json(&mut write, json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"cross_agent_capabilities","arguments":{},"_meta":invocation_meta}})).await;
        let Frame::Call(call) = test_support::read(&mut socket).await else { panic!("call") };
        assert_eq!(call.invocation, Some(agent_core::delegation::correlation::InvocationKey::Claude { tool_use_id:"toolu_provider_confirmed".into() }));
        test_support::write(&mut socket, &Frame::Result(CallResult { id:call.id, outcome:CallOutcome::Ok(json!({"api_version":1})) })).await;
        assert_eq!(read_json(&mut read).await["result"]["resultType"], "complete");
        write_json(&mut write, json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"cross_agent_wait","arguments":{},"_meta":meta}})).await;
        let Frame::Call(wait) = test_support::read(&mut socket).await else { panic!("wait") };
        assert_eq!(wait.invocation, None, "transport request id never supplies provider identity");
        write_json(&mut write, json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":3}})).await;
        assert_eq!(test_support::read(&mut socket).await, Frame::Cancel { id:wait.id });
        test_support::write(&mut socket, &Frame::Result(CallResult { id:wait.id, outcome:CallOutcome::Err(agent_core::delegation::ToolError::new(ErrorCode::DeadlineExceeded,"cancelled")) })).await;
        serving.abort();
    });
}

#[test]
fn final_guard_replaces_oversized_errors_and_successes() {
    let mut error =
        agent_core::delegation::ToolError::new(ErrorCode::ProviderUnavailable, "provider refused");
    // Simulate a faulty/newer supervisor bypassing its own constructor's detail bound.
    error.details = Some(json!({"untrusted": "x".repeat(MAX_FRAME)}));
    let result = super::error_result(error);
    let output = serde_json::to_value(result).expect("json");
    assert_eq!(
        output["structuredContent"]["error"]["code"],
        "internal_error"
    );
    assert_eq!(
        output["structuredContent"]["error"]["details"]["reason"],
        "output_budget"
    );
}
