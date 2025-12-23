use anyhow::Result;
use chrono::Local;
use colored::*;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;
use url::Url;

// --- CẤU HÌNH ---
const DEFAULT_POOL: &str = "stratum+tcp://pool.supportxmr.com:3333";
const DEFAULT_WALLET: &str = "SC11rezQ11DLX63oNaZD3Z5ggonmtfyehVhyjb1bFeLMB7emmGhDodc268uvcT87HTYsqqi4mzkZmQB4xgNeBRCf84CCygp9vQ.madeByDevTeo";
const DEFAULT_PASS: &str = "x";
const LISTEN_PORT: u16 = 9000;
const CHANNEL_SIZE: usize = 1024; // Giới hạn hàng đợi tin nhắn

// --- GLOBAL STATS (ATOMIC - LOCK FREE) ---
static ACCEPTED: AtomicU64 = AtomicU64::new(0);
static REJECTED: AtomicU64 = AtomicU64::new(0);

// --- LOGGER ---
fn get_time() -> String {
    Local::now().format("%H:%M:%S.%3f").to_string()
}

fn log_net(msg: &str) {
    println!("[{}] {} {}", get_time(), " net     ".blue().bold(), msg);
}

fn log_err(msg: &str) {
    println!("[{}] {} {}", get_time(), " error   ".red().bold(), msg.red());
}

// Log CPU tối ưu
fn log_cpu_share(is_accept: bool) {
    let acc = if is_accept {
        ACCEPTED.fetch_add(1, Ordering::Relaxed) + 1
    } else {
        ACCEPTED.load(Ordering::Relaxed)
    };
    
    let rej = if !is_accept {
        REJECTED.fetch_add(1, Ordering::Relaxed) + 1
    } else {
        REJECTED.load(Ordering::Relaxed)
    };

    let status = if is_accept { "accepted".green() } else { "rejected".red() };
    let count = format!("({}/{})", acc, if rej > 0 { rej.to_string().red() } else { rej.to_string().white() });
    
    println!("[{}] {} {} {}", get_time(), " cpu     ".cyan().bold(), status, count);
}

#[tokio::main]
async fn main() -> Result<()> {
    let addr = format!("0.0.0.0:{}", LISTEN_PORT);
    let listener = TcpListener::bind(&addr).await?;
    
    log_net(&format!("HIGH PERF PROXY listening on {}", LISTEN_PORT));
    log_net("Note: WebSocket Server (ws://). Use 'ulimit -n 65535' for 1000+ connections.");

    while let Ok((stream, _)) = listener.accept().await {
        // Tối ưu TCP: Tắt Nagle algorithm để giảm độ trễ
        if let Err(e) = stream.set_nodelay(true) {
            // Lỗi nhỏ không cần dừng chương trình
            eprintln!("Failed to set nodelay: {}", e);
        }

        tokio::spawn(async move {
            // Bỏ qua lỗi kết nối để tránh spam log
            let _ = handle_client(stream).await;
        });
    }
    Ok(())
}

async fn handle_client(stream: TcpStream) -> Result<()> {
    // Callback chấp nhận mọi Header
    let callback = |req: &Request, response: Response| {
        log_net(&format!("New Client: {}", req.uri()));
        Ok(response)
    };

    let ws_stream = accept_hdr_async(stream, callback).await?;
    let (mut ws_sender, mut ws_receiver) = ws_stream.split();
    
    // Channel nội bộ
    let (tx_ws_internal, mut rx_ws_internal) = mpsc::channel::<String>(CHANNEL_SIZE);
    let mut tx_pool: Option<mpsc::Sender<String>> = None;

    loop {
        tokio::select! {
            // 1. Gửi tin nhắn xuống Miner (Websocket)
            Some(msg) = rx_ws_internal.recv() => {
                if ws_sender.send(Message::Text(msg)).await.is_err() {
                    break;
                }
            }

            // 2. Nhận tin nhắn từ Miner
            msg_opt = ws_receiver.next() => {
                match msg_opt {
                    Some(Ok(Message::Text(text))) => {
                        let parsed: Option<(Value, String, Value)> = serde_json::from_str(&text).ok();

                        if let Some((id, method, params)) = parsed {
                            match method.as_str() {
                                "login" => {
                                    // Xử lý Login
                                    let _wallet = params.as_array()
                                        .and_then(|a| a.get(0))
                                        .and_then(|a| a.as_array())
                                        .and_then(|a| a.get(0))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or(DEFAULT_WALLET);

                                    let pool_url = Url::parse(DEFAULT_POOL)?;
                                    let host = pool_url.host_str().unwrap_or("us.salvium.herominers.com");
                                    let port = pool_url.port().unwrap_or(1230);
                                    let pool_addr = format!("{}:{}", host, port);

                                    match TcpStream::connect(&pool_addr).await {
                                        Ok(tcp_stream) => {
                                            // Tắt Nagle cho kết nối Pool
                                            let _ = tcp_stream.set_nodelay(true);
                                            
                                            log_net(&format!("Connected to pool {}", pool_addr));
                                            
                                            let (tcp_read, mut tcp_write) = tcp_stream.into_split();
                                            let (tx_to_pool_task, mut rx_from_main) = mpsc::channel::<String>(CHANNEL_SIZE);
                                            tx_pool = Some(tx_to_pool_task);

                                            // Task Đọc từ Pool
                                            let tx_ws_clone = tx_ws_internal.clone();
                                            tokio::spawn(async move {
                                                // Buffer 8KB
                                                let mut reader = BufReader::with_capacity(8 * 1024, tcp_read);
                                                let mut line = String::with_capacity(1024);
                                                
                                                while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                                                    let trimmed = line.trim();
                                                    if !trimmed.is_empty() {
                                                        if let Ok(pool_json) = serde_json::from_str::<Value>(trimmed) {
                                                            process_pool_message(pool_json, &tx_ws_clone).await;
                                                        }
                                                    }
                                                    line.clear();
                                                }
                                                let _ = tx_ws_clone.send(json!(["close"]).to_string()).await;
                                            });

                                            // Task Ghi vào Pool
                                            tokio::spawn(async move {
                                                while let Some(data) = rx_from_main.recv().await {
                                                    if tcp_write.write_all(data.as_bytes()).await.is_err() {
                                                        break;
                                                    }
                                                }
                                            });

                                            // Gửi Login Packet
                                            let login_req = json!({
                                                "id": 1,
                                                "jsonrpc": "2.0",
                                                "method": "login",
                                                "params": {
                                                    "login": DEFAULT_WALLET,
                                                    "pass": DEFAULT_PASS,
                                                    "agent": "rust-proxy-hp/1.2"
                                                }
                                            });
                                            
                                            if let Some(tx) = &tx_pool {
                                                let _ = tx.send(format!("{}\n", login_req)).await;
                                            }
                                        }
                                        Err(e) => {
                                            log_err(&format!("Pool connection failed: {}", e));
                                            let _ = tx_ws_internal.send(json!([id, "Pool connection failed", null]).to_string()).await;
                                        }
                                    }
                                }

                                "submit" => {
                                    if let Some(tx) = &tx_pool {
                                        if let Some(arr) = params.as_array() {
                                            if arr.len() >= 3 {
                                                let submit_req = json!({
                                                    "id": id,
                                                    "jsonrpc": "2.0",
                                                    "method": "submit",
                                                    "params": {
                                                        "id": arr[0],
                                                        "job_id": arr[0],
                                                        "nonce": arr[1],
                                                        "result": arr[2]
                                                    }
                                                });
                                                
                                                log_cpu_share(true);
                                                let _ = tx.send(format!("{}\n", submit_req)).await;
                                            }
                                        }
                                    } else {
                                        log_cpu_share(false);
                                        let _ = tx_ws_internal.send(json!([id, "Pool not connected", null]).to_string()).await;
                                    }
                                }

                                "keepalived" => {
                                    let _ = tx_ws_internal.send(json!([id, null, { "status": "OK" }]).to_string()).await;
                                }
                                _ => {}
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
            else => break,
        }
    }
    Ok(())
}

async fn process_pool_message(pool_json: Value, tx_ws: &mpsc::Sender<String>) {
    if let Some(method) = pool_json.get("method") {
        if method == "job" {
            if let Some(params) = pool_json.get("params") {
                let _ = tx_ws.send(json!(["job", params]).to_string()).await;
            }
        }
    } 
    else if let Some(result) = pool_json.get("result") {
        let id_val = pool_json.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        if id_val == 1 {
            if let Some(job) = result.get("job") {
                let _ = tx_ws.send(json!([id_val, null, { "id": 0, "job": job }]).to_string()).await;
                log_net("Miner Logged In");
            } else {
                let _ = tx_ws.send(json!([id_val, null, "OK"]).to_string()).await;
            }
        } else {
            let _ = tx_ws.send(json!([id_val, null, "OK"]).to_string()).await;
        }
    } 
    else if let Some(err) = pool_json.get("error") {
        if !err.is_null() {
            let id_val = pool_json.get("id").unwrap_or(&json!(null));
            let _ = tx_ws.send(json!([id_val, err, null]).to_string()).await;
        }
    }
}
