use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::env;
use tower_http::services::ServeDir;
use tower_http::cors::CorsLayer;
use open;

#[tokio::main]
async fn main() {
    let app = Router::new()
        .nest_service("/", ServeDir::new("frontend/dist"))
        .route("/ws", get(ws_handler))
        .layer(CorsLayer::permissive());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3001").await.unwrap();
    println!("Backend listening on http://127.0.0.1:3001");
    
    // Open the browser
    let _ = open::that("http://127.0.0.1:3001");

    axum::serve(listener, app).await.unwrap();
}

async fn ws_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(handle_socket)
}

async fn handle_socket(socket: WebSocket) {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    
    // Check if gemini exists
    let has_gemini = is_gemini_available();
    let cmd = if has_gemini {
        vec!["-ic", "gemini"]
    } else {
        vec!["-ic", "exec $SHELL"]
    };

    let mut cmd_builder = CommandBuilder::new(&shell);
    cmd_builder.args(&cmd);
    cmd_builder.cwd(env::var("HOME").unwrap_or_else(|_| "/".to_string()));

    let mut child = pair.slave.spawn_command(cmd_builder).unwrap();

    let mut reader = pair.master.try_clone_reader().unwrap();
    let writer = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));

    // Thread to read from PTY and send to WebSocket
    let (mut sender, mut receiver) = socket.split();
    
    tokio::spawn(async move {
        let mut buffer = [0u8; 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(n) if n > 0 => {
                    let msg = String::from_utf8_lossy(&buffer[..n]).to_string();
                    if sender.send(Message::Text(msg)).await.is_err() {
                        break;
                    }
                }
                _ => break,
            }
        }
    });

    // Handle incoming messages from WebSocket
    while let Some(Ok(msg)) = receiver.next().await {
        if let Message::Text(text) = msg {
            let mut w = writer.lock().unwrap();
            let _ = w.write_all(text.as_bytes());
        }
    }

    let _ = child.kill();
}

fn is_gemini_available() -> bool {
    if std::process::Command::new("which")
        .arg("gemini")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return true;
    }

    let home = env::var("HOME").unwrap_or_default();
    let paths = [
        "/usr/bin/gemini",
        "/usr/local/bin/gemini",
        &format!("{}/.local/bin/gemini", home),
        &format!("{}/.npm-global/bin/gemini", home),
        &format!("{}/bin/gemini", home),
    ];

    for path in paths {
        if !path.is_empty() && std::path::Path::new(path).exists() {
            return true;
        }
    }

    let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    std::process::Command::new(shell)
        .args(["-ic", "command -v gemini"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
