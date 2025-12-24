use anyhow::Result;
use chrono::Local;
use colored::*;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::accept_async; // Dùng hàm accept đơn giản hơn
use tokio_tungstenite::tungstenite::Message;
use url::Url;

// --- CẤU HÌNH ---
const DEFAULT_POOL: &str = "stratum+tcp://pool.supportxmr.com:8080";
const DEFAULT_WALLET: &str = "44hQZfLkTccVGood4aYMTm1KPyJVoa9esLyq1bneAvhkchQdmFTx3rsD3KRwpXTUPd1iTF4VVGYsTCLYrxMZVsvtKqAmBiw";
const DEFAULT_PASS: &str = "ProxyWorker";
const LISTEN_PORT: u16 = 9000;
const CHANNEL_SIZE: usize = 2048;

// --- STATS ---
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

fn log_share(is_valid: bool) {
    if is_valid {
        let count = ACCEPTED.fetch_add(1, Ordering::Relaxed) + 1;
        let rej = REJECTED.load(Ordering::Relaxed);
        println!("[{}] {} accepted ({}/{})", get_time(), " cpu     ".green().bold(), count, rej);
    } else {
        let count = ACCEPTED.load(Ordering::Relaxed);
        let rej = REJECTED.fetch_add(1, Ordering::Relaxed) + 1;
        println!("[{}] {} rejected ({}/{})", get_time(), " cpu     ".red().bold(), count, rej);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let addr = format!("0.0.0.0:{}", LISTEN_PORT);
    let listener = TcpListener::bind(&addr).await?;
    
    log_net(&format!("RANDOMX PROXY (Easy Connect) listening on {}", LISTEN_PORT));
    
    while let Ok((stream, _)) = listener.accept().await {
        // Tối ưu TCP cơ bản
        let _ = stream.set_nodelay(true);

        tokio::spawn(async move {
            if let Err(e) = handle_client(stream).await {
                // Lọc bớt lỗi handshake để log sạch hơn
                let msg = e.to_string();
                if !msg.contains("Handshake") && !msg.contains("Connection reset") {
                    log_err(&format!("Client Error: {}", msg));
                }
            }
        });
    }
    Ok(())
}

async fn handle_client(stream: TcpStream) -> Result<()> {
    // --- THAY ĐỔI: Dùng accept_async mặc định (Dễ tính hơn với Header) ---
    // Bỏ qua callback phức tạp, chấp nhận mọi kết nối WS
    let ws_stream = accept_async(stream).await?;
    
    log_net("New Client Connected"); // Log đơn giản

    let (mut ws_sender, mut ws_receiver) = ws_stream.split();
    
    let (tx_ws_internal, mut rx_ws_internal) = mpsc::channel::<String>(CHANNEL_SIZE);
    let mut tx_pool: Option<mpsc::Sender<String>> = None;

    // Session ID Storage
    let pool_session_id = Arc::new(Mutex::new(String::new()));

    loop {
        tokio::select! {
            // 1. Gửi tin xuống Miner
            Some(msg) = rx_ws_internal.recv() => {
                if ws_sender.send(Message::Text(msg)).await.is_err() {
                    break;
                }
            }

            // 2. Nhận tin từ Miner
            msg_opt = ws_receiver.next() => {
                match msg_opt {
                    Some(Ok(Message::Text(text))) => {
                        let parsed: Option<(Value, String, Value)> = serde_json::from_str(&text).ok();

                        if let Some((id, method, params)) = parsed {
                            match method.as_str() {
                                "login" => {
                                    let login_wallet = params.as_array()
                                        .and_then(|a| a.get(0))
                                        .and_then(|a| a.as_array())
                                        .and_then(|a| a.get(0))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or(DEFAULT_WALLET);

                                    // Kết nối Pool
                                    let pool_url = Url::parse(DEFAULT_POOL)?;
                                    let host = pool_url.host_str().unwrap_or("pool.supportxmr.com");
                                    let port = pool_url.port().unwrap_or(8080);
                                    let pool_addr = format!("{}:{}", host, port);

                                    match TcpStream::connect(&pool_addr).await {
                                        Ok(tcp_stream) => {
                                            let _ = tcp_stream.set_nodelay(true);
                                            log_net(&format!("Connected to pool {}", pool_addr));
                                            
                                            let (tcp_read, mut tcp_write) = tcp_stream.into_split();
                                            let (tx_to_pool_task, mut rx_from_main) = mpsc::channel::<String>(CHANNEL_SIZE);
                                            tx_pool = Some(tx_to_pool_task);

                                            let tx_ws_clone = tx_ws_internal.clone();
                                            let session_clone = pool_session_id.clone();

                                            tokio::spawn(async move {
                                                let mut reader = BufReader::with_capacity(8 * 1024, tcp_read);
                                                let mut line = String::with_capacity(2048);
                                                while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                                                    let trimmed = line.trim();
                                                    if !trimmed.is_empty() {
                                                        if let Ok(json) = serde_json::from_str::<Value>(trimmed) {
                                                            process_pool_message(json, &tx_ws_clone, &session_clone).await;
                                                        }
                                                    }
                                                    line.clear();
                                                }
                                                let _ = tx_ws_clone.send(json!(["close"]).to_string()).await;
                                            });

                                            tokio::spawn(async move {
                                                while let Some(data) = rx_from_main.recv().await {
                                                    if tcp_write.write_all(data.as_bytes()).await.is_err() {
                                                        break;
                                                    }
                                                }
                                            });

                                            // --- LOGIC QUAN TRỌNG: GÓI TIN CHUẨN ĐỂ KHÔNG BỊ BAN ---
                                            let login_req = json!({
                                                "id": 1,
                                                "jsonrpc": "2.0",
                                                "method": "login",
                                                "params": {
                                                    "login": login_wallet,
                                                    "pass": DEFAULT_PASS,
                                                    "agent": "XMRig/6.22.0 (Proxy)", // Giả danh XMRig
                                                    "algo": ["rx/0"],                // Bắt buộc cho RandomX
                                                    "rigid": "proxy"
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
                                                let worker_id = {
                                                    let lock = pool_session_id.lock().unwrap();
                                                    lock.clone()
                                                };

                                                // Chỉ gửi khi đã có Worker ID (Login xong)
                                                if !worker_id.is_empty() {
                                                    let submit_req = json!({
                                                        "id": id,
                                                        "jsonrpc": "2.0",
                                                        "method": "submit",
                                                        "params": {
                                                            "id": worker_id,   // ID đúng từ Pool
                                                            "job_id": arr[0],  // Job ID từ Miner
                                                            "nonce": arr[1],
                                                            "result": arr[2],
                                                            "algo": "rx/0"
                                                        }
                                                    });
                                                    
                                                    log_share(true);
                                                    let _ = tx.send(format!("{}\n", submit_req)).await;
                                                }
                                            }
                                        }
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

async fn process_pool_message(pool_json: Value, tx_ws: &mpsc::Sender<String>, session_storage: &Arc<Mutex<String>>) {
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
            // LƯU SESSION ID
            if let Some(sid) = result.get("id") {
                if let Some(s) = sid.as_str() {
                    let mut lock = session_storage.lock().unwrap();
                    *lock = s.to_string();
                }
            }
            if let Some(job) = result.get("job") {
                let _ = tx_ws.send(json!([id_val, null, { "id": 0, "job": job }]).to_string()).await;
                log_net("Miner Logged In");
            } else {
                let _ = tx_ws.send(json!([id_val, null, "OK"]).to_string()).await;
            }
        } else {
            // SUBMIT RESPONSE
            if let Some(status) = result.get("status") {
                 if status != "OK" {
                     log_share(false); // Rejected
                 }
            }
            let _ = tx_ws.send(json!([id_val, null, "OK"]).to_string()).await;
        }
    } 
    else if let Some(err) = pool_json.get("error") {
        if !err.is_null() {
            let id_val = pool_json.get("id").unwrap_or(&json!(null));
            if let Some(msg) = err.get("message") {
                log_err(&format!("Pool Error: {}", msg));
            }
            let _ = tx_ws.send(json!([id_val, err, null]).to_string()).await;
        }
    }
}
