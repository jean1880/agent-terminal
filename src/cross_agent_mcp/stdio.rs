//! Bounded newline framing before rmcp sees a message. The SDK's default stdio transport
//! reads an unbounded line and can echo an unbounded request id, so neither is used here.
use std::collections::BTreeSet;
use std::future::Future;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use agent_core::delegation::ipc::MAX_FRAME;
use agent_core::delegation::wire::request_id_ok;
use rmcp::service::{RoleServer, RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

type Cancel = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct ActiveRequest {
    callback: Option<Cancel>,
    cancelled: bool,
}

/// rmcp's 2026 lifecycle only interprets cancellation for subscriptions. Delegation waits
/// still need cancellation, so the transport also signals the exact active request's token.
#[derive(Clone, Default)]
pub(super) struct CancellationRegistry(
    Arc<std::sync::Mutex<std::collections::BTreeMap<String, ActiveRequest>>>,
);

impl CancellationRegistry {
    pub(super) fn register(
        &self,
        id: String,
        cancel: impl Fn() + Send + Sync + 'static,
    ) -> Registration {
        let cancel: Cancel = Arc::new(cancel);
        let already_cancelled = self
            .0
            .lock()
            .ok()
            .map(|mut active| {
                let entry = active.entry(id.clone()).or_default();
                entry.callback = Some(cancel.clone());
                entry.cancelled
            })
            .unwrap_or(true);
        if already_cancelled {
            cancel();
        }
        Registration {
            id,
            registry: self.clone(),
            callback: cancel,
        }
    }

    fn incoming(&self, id: String) {
        if let Ok(mut active) = self.0.lock() {
            active.insert(id, ActiveRequest::default());
        }
    }

    fn cancel(&self, id: &str) {
        let callback = self.0.lock().ok().and_then(|mut active| {
            let entry = active.get_mut(id)?;
            entry.cancelled = true;
            entry.callback.clone()
        });
        if let Some(cancel) = callback {
            cancel();
        }
    }

    fn completed(&self, id: &str) {
        if let Ok(mut active) = self.0.lock() {
            active.remove(id);
        }
    }

    #[cfg(test)]
    pub(super) fn active_count(&self) -> usize {
        self.0.lock().expect("registry lock").len()
    }
}

pub(super) struct Registration {
    id: String,
    registry: CancellationRegistry,
    callback: Cancel,
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Ok(mut active) = self.registry.0.lock() {
            // A response may finish before its handler's guard drops. Never remove a
            // subsequently reused id's new cancellation entry.
            if active
                .get(&self.id)
                .and_then(|entry| entry.callback.as_ref())
                .is_some_and(|callback| Arc::ptr_eq(callback, &self.callback))
            {
                active.remove(&self.id);
            }
        }
    }
}

/// Also bounds SDK handler tasks while the eager hello is pending. Four calls reach IPC;
/// lifecycle and discovery requests share this modest bridge-only ceiling.
const MAX_PENDING: usize = 16;

pub(super) type BoundedStdio = BoundedTransport<tokio::io::Stdin, tokio::io::Stdout>;

impl BoundedStdio {
    pub(super) fn new(cancellations: CancellationRegistry) -> Self {
        Self::with_io(tokio::io::stdin(), tokio::io::stdout()).with_cancellations(cancellations)
    }
}

pub(super) struct BoundedTransport<R, W> {
    reader: BufReader<R>,
    /// Survives cancellation of `receive`, including a partial line.
    line: Vec<u8>,
    writer: Arc<Mutex<Option<W>>>,
    pending: Arc<std::sync::Mutex<BTreeSet<String>>>,
    cancellations: CancellationRegistry,
    ended: bool,
}

impl<R: AsyncRead, W: AsyncWrite> BoundedTransport<R, W> {
    pub(super) fn with_io(reader: R, writer: W) -> Self {
        Self {
            reader: BufReader::new(reader),
            line: Vec::new(),
            writer: Arc::new(Mutex::new(Some(writer))),
            pending: Arc::default(),
            cancellations: CancellationRegistry::default(),
            ended: false,
        }
    }

    pub(super) fn with_cancellations(mut self, registry: CancellationRegistry) -> Self {
        self.cancellations = registry;
        self
    }
}

impl<R, W: AsyncWrite + Unpin> BoundedTransport<R, W> {
    /// Writes a reply the transport answers by itself. A reply that cannot be written means the
    /// transport is dead: it is marked ended and `None` is returned.
    async fn reply_or_end(&mut self, value: Value) -> Option<()> {
        if write_value(&self.writer, value).await.is_err() {
            self.ended = true;
            return None;
        }
        Some(())
    }
}

async fn write_value<W: AsyncWrite + Unpin>(
    writer: &Arc<Mutex<Option<W>>>,
    value: Value,
) -> io::Result<()> {
    let mut bytes =
        serde_json::to_vec(&value).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
    if bytes.len() >= MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "MCP response exceeds its frame budget",
        ));
    }
    bytes.push(b'\n');
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut guard = writer.lock().await;
        let writer = guard
            .as_mut()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotConnected))?;
        writer.write_all(&bytes).await?;
        writer.flush().await
    })
    .await
    .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?
}

fn rpc_error(code: i32, message: &str, id: Value) -> Value {
    json!({"jsonrpc":"2.0", "id": id, "error":{"code":code,"message":message}})
}

/// Checks the response id and a cancellation's target before typed SDK dispatch. It never
/// serialises the full request into an error or includes a rejected id in a response.
pub(super) fn bounded_ids(value: &Value) -> bool {
    let within =
        |id: &Value| serde_json::to_string(id).is_ok_and(|encoded| request_id_ok(&encoded));
    value.get("id").is_none_or(within)
        && (value.get("method").and_then(Value::as_str) != Some("notifications/cancelled")
            || value
                .get("params")
                .and_then(|params| params.get("requestId"))
                .is_none_or(within))
}

/// Measure the original encoding as well: serde_json normalises numeric exponents and
/// Unicode escapes, which must not make a 257-byte wire id pass the 256-byte bound.
pub(super) fn bounded_wire_ids(line: &[u8]) -> bool {
    let mut valid = true;
    let _ = visit_field(line, "id", &mut |id| valid &= id.len() <= 256);
    let _ = visit_field(line, "params", &mut |params| {
        let _ = visit_field(params, "requestId", &mut |id| valid &= id.len() <= 256);
    });
    valid
}

fn visit_field(object: &[u8], wanted: &str, visitor: &mut dyn FnMut(&[u8])) -> Option<()> {
    fn ws(bytes: &[u8], mut at: usize) -> usize {
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        at
    }
    fn end(bytes: &[u8], at: usize) -> Option<usize> {
        match *bytes.get(at)? {
            b'"' => {
                let mut cursor = at + 1;
                while let Some(byte) = bytes.get(cursor) {
                    match byte {
                        b'\\' => cursor += 2,
                        b'"' => return Some(cursor + 1),
                        _ => cursor += 1,
                    }
                }
                None
            }
            b'{' | b'[' => {
                let mut cursor = at + 1;
                let mut depth = 1usize;
                while let Some(byte) = bytes.get(cursor) {
                    match byte {
                        b'"' => cursor = end(bytes, cursor)?,
                        b'{' | b'[' => {
                            depth += 1;
                            cursor += 1;
                        }
                        b'}' | b']' => {
                            depth -= 1;
                            cursor += 1;
                            if depth == 0 {
                                return Some(cursor);
                            }
                        }
                        _ => cursor += 1,
                    }
                }
                None
            }
            _ => {
                let mut cursor = at;
                while bytes
                    .get(cursor)
                    .is_some_and(|byte| !byte.is_ascii_whitespace() && !b",]}".contains(byte))
                {
                    cursor += 1;
                }
                (cursor > at).then_some(cursor)
            }
        }
    }
    let mut cursor = ws(object, 0);
    if object.get(cursor) != Some(&b'{') {
        return None;
    }
    cursor += 1;
    loop {
        cursor = ws(object, cursor);
        if object.get(cursor) == Some(&b'}') {
            return Some(());
        }
        if object.get(cursor) != Some(&b'"') {
            return None;
        }
        let key_end = end(object, cursor)?;
        let key: String = serde_json::from_slice(&object[cursor..key_end]).ok()?;
        cursor = ws(object, key_end);
        if object.get(cursor) != Some(&b':') {
            return None;
        }
        cursor = ws(object, cursor + 1);
        let value_end = end(object, cursor)?;
        if key == wanted {
            visitor(&object[cursor..value_end]);
        }
        cursor = ws(object, value_end);
        if object.get(cursor) == Some(&b'}') {
            return Some(());
        }
        if object.get(cursor) != Some(&b',') {
            return None;
        }
        cursor += 1;
    }
}

impl<R, W> Transport<RoleServer> for BoundedTransport<R, W>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send + 'static,
{
    type Error = io::Error;

    fn send(
        &mut self,
        message: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = io::Result<()>> + Send + 'static {
        let writer = self.writer.clone();
        let pending = self.pending.clone();
        let cancellations = self.cancellations.clone();
        async move {
            let value = serde_json::to_value(message)
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
            if let Some(id) = value
                .get("id")
                .filter(|_| value.get("result").is_some() || value.get("error").is_some())
                .and_then(|id| serde_json::to_string(id).ok())
            {
                if let Ok(mut pending) = pending.lock() {
                    pending.remove(&id);
                    // SDK lifecycle/metadata validation can reject a tools/call before
                    // call_tool creates its Registration guard. Replies own cleanup too.
                    cancellations.completed(&id);
                }
            }
            write_value(&writer, value).await
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleServer>> {
        if self.ended {
            return None;
        }
        loop {
            // fill_buf/consume are cancellation safe; reserve only what this line needs.
            loop {
                let chunk = match self.reader.fill_buf().await {
                    Ok(chunk) if !chunk.is_empty() => chunk,
                    _ => {
                        self.ended = true;
                        return None;
                    }
                };
                let end = chunk.iter().position(|byte| *byte == b'\n');
                let taken = end.map_or(chunk.len(), |index| index + 1);
                let payload = taken - usize::from(end.is_some());
                if self.line.len().saturating_add(payload) > MAX_FRAME {
                    // Oversize terminates the bridge: never try to resynchronise.
                    self.ended = true;
                    return None;
                }
                self.line.extend_from_slice(&chunk[..taken]);
                self.reader.consume(taken);
                if end.is_some() {
                    break;
                }
            }
            let line = std::mem::take(&mut self.line);
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            if !bounded_wire_ids(&line) {
                self.reply_or_end(rpc_error(
                    -32600,
                    "request id exceeds 256 encoded bytes",
                    Value::Null,
                ))
                .await?;
                continue;
            }
            let value: Value = match serde_json::from_slice(&line) {
                Ok(value) => value,
                Err(_) => {
                    // Ignore malformed bytes, matching the SDK's stdio compatibility
                    // behaviour. No parser error can disclose input text.
                    continue;
                }
            };
            if !bounded_ids(&value) {
                self.reply_or_end(rpc_error(
                    -32600,
                    "request id exceeds 256 encoded bytes",
                    Value::Null,
                ))
                .await?;
                continue;
            }
            if value
                .get("id")
                .is_some_and(|id| !id.is_string() && id.as_i64().is_none())
            {
                self.reply_or_end(rpc_error(-32600, "invalid MCP request id", Value::Null))
                    .await?;
                continue;
            }
            if value.get("method").and_then(Value::as_str) == Some("notifications/cancelled") {
                if let Some(id) = value
                    .get("params")
                    .and_then(|params| params.get("requestId"))
                    .and_then(|id| serde_json::to_string(id).ok())
                {
                    self.cancellations.cancel(&id);
                    // Legacy SDK lifecycles may end cancelled requests without a reply.
                    if let Ok(mut pending) = self.pending.lock() {
                        pending.remove(&id);
                    }
                }
            }
            // A non-object `arguments` cannot reach rmcp's typed handler. Return the same
            // schema error here instead of misclassifying it as an unknown JSON-RPC method.
            if value.get("method").and_then(Value::as_str) == Some("tools/call") {
                if let Some(arguments) = value
                    .get("params")
                    .and_then(|params| params.get("arguments"))
                {
                    if !arguments.is_object() {
                        if value
                            .pointer("/params/name")
                            .and_then(Value::as_str)
                            .and_then(agent_core::delegation::ToolName::parse)
                            .is_none()
                        {
                            if let Some(id) = value.get("id") {
                                self.reply_or_end(rpc_error(
                                    -32602,
                                    "unknown delegation tool",
                                    id.clone(),
                                ))
                                .await?;
                            }
                            continue;
                        }
                        let result =
                            super::error_result(agent_core::delegation::ToolError::invalid(
                                "arguments",
                                "not_an_object",
                            ));
                        let mut result = serde_json::to_value(result).ok()?;
                        if value
                            .pointer("/params/_meta/io.modelcontextprotocol~1protocolVersion")
                            .and_then(Value::as_str)
                            .is_some_and(|version| version >= "2026-07-28")
                        {
                            result["resultType"] = json!("complete");
                        }
                        if let Some(id) = value.get("id") {
                            self.reply_or_end(json!({"jsonrpc":"2.0", "id":id, "result":result}))
                                .await?;
                        }
                        continue;
                    }
                }
            }
            let id = value.get("id").cloned();
            let parsed = match serde_json::from_value(value.clone()) {
                Ok(parsed) => parsed,
                Err(_) => {
                    if let Some(id) = id {
                        let code = match value.get("method").and_then(Value::as_str) {
                            Some(
                                "tools/call" | "tools/list" | "initialize" | "server/discover"
                                | "ping",
                            ) => -32602,
                            Some(_) => -32601,
                            None => -32600,
                        };
                        self.reply_or_end(rpc_error(
                            code,
                            "unsupported or invalid MCP request",
                            id,
                        ))
                        .await?;
                    }
                    continue;
                }
            };
            if let Some(id) = id.filter(|_| value.get("method").is_some()) {
                let encoded = serde_json::to_string(&id).ok()?;
                let admitted = {
                    let Ok(mut pending) = self.pending.lock() else {
                        return None;
                    };
                    if pending.len() >= MAX_PENDING || pending.contains(&encoded) {
                        false
                    } else {
                        pending.insert(encoded.clone());
                        true
                    }
                };
                if !admitted {
                    self.reply_or_end(rpc_error(
                        -32000,
                        "too many pending MCP requests or duplicate id",
                        id,
                    ))
                    .await?;
                    continue;
                }
                if value.get("method").and_then(Value::as_str) == Some("tools/call") {
                    self.cancellations.incoming(encoded);
                }
            }
            return Some(parsed);
        }
    }

    async fn close(&mut self) -> io::Result<()> {
        self.ended = true;
        self.writer.lock().await.take();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds `input` to a transport whose reader has gone, and reports what two receives see.
    fn receive_twice_with_a_dead_peer(input: &str) -> (bool, bool) {
        tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("runtime")
            .block_on(async {
                let (output, peer) = tokio::io::duplex(64);
                drop(peer);
                let mut transport = BoundedTransport::with_io(input.as_bytes(), output);
                let first = Transport::<RoleServer>::receive(&mut transport).await;
                let second = Transport::<RoleServer>::receive(&mut transport).await;
                (first.is_some(), second.is_some())
            })
    }

    #[test]
    fn a_reply_that_cannot_be_written_ends_the_transport() {
        let ping = "{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"ping\"}\n";
        let replied_by_the_transport = [
            // An unknown tool with arguments that are not an object.
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"nope\",\"arguments\":3}}\n",
            // A known tool with arguments that are not an object.
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"cross_agent_list\",\"arguments\":3}}\n",
            // An id that is neither a string nor an integer.
            "{\"jsonrpc\":\"2.0\",\"id\":1.5,\"method\":\"ping\"}\n",
        ];
        for request in replied_by_the_transport {
            let input = format!("{request}{ping}");
            assert_eq!(
                receive_twice_with_a_dead_peer(&input),
                (false, false),
                "{request}"
            );
        }
    }
}
