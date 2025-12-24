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
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;
use url::Url;

// --- CẤU HÌNH ---
const DEFAULT_POOL: &str = "stratum+tcp://pool.supportxmr.com:8080";
const DEFAULT_WALLET: &str = "44hQZfLkTccVGood4aYMTm1KPyJVoa9esLyq1bneAvhkchQdmFTx3rsD3KRwpXTUPd1iTF4VVGYsTCLYrxMZVsvtKqAmBiw";
const DEFAULT_PASS: &str = "ProxyWorker";
const LISTEN_PORT: u16 = 9000;
const CHANNEL_SIZE: usize = 4096; // Tăng buffer

// --- STATS ---
static ACCEPTED: AtomicU64 = AtomicU64::new(0);
static REJECTED: AtomicU64 = AtomicU64::new(0);

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
    
    log_net(&format!("UNIVERSAL PROXY listening on {}", LISTEN_PORT));
    
    while let Ok((stream, _)) = listener.accept().await {
        let _ = stream.set_nodelay(true);
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream).await {
                let msg = e.to_string();
                if !msg.contains("Handshake") && !msg.contains("Connection reset") && !msg.contains("Closed") {
                    log_err(&format!("Client Error: {}", msg));
                }
            }
        });
    }
    Ok(())
}

async fn handle_client(stream: TcpStream) -> Result<()> {
    let ws_stream = accept_async(stream).await?;
    log_net("New Client Connected (WebSocket)");

    let (mut ws_sender, mut ws_receiver) = ws_stream.split();
    let (tx_ws_internal, mut rx_ws_internal) = mpsc::channel::<String>(CHANNEL_SIZE);
    let mut tx_pool: Option<mpsc::Sender<String>> = None;

    // Lưu Session ID của Pool (để gửi Submit)
    let pool_session_id = Arc::new(Mutex::new(String::new()));
    // Lưu ID gốc của Miner (để trả lời Login cho đúng)
    let miner_login_id = Arc::new(Mutex::new(Value::Null));

    loop {
        tokio::select! {
            Some(msg) = rx_ws_internal.recv() => {
                if ws_sender.send(Message::Text(msg)).await.is_err() { break; }
            }

            msg_opt = ws_receiver.next() => {
                match msg_opt {
                    Some(Ok(Message::Text(text))) => {
                        // --- FIX QUAN TRỌNG: Parse linh hoạt (Object hoặc Array) ---
                        let json_val: Result<Value, _> = serde_json::from_str(&text);
                        
                        if let Ok(val) = json_val {
                            // Chuẩn hóa dữ liệu về dạng (id, method, params)
                            let (id, method, params) = parse_rpc(&val);

                            if let Some(m) = method {
                                match m.as_str() {
                                    "login" => {
                                        // 1. Lưu ID gốc của Miner
                                        {
                                            let mut lock = miner_login_id.lock().unwrap();
                                            *lock = id.clone();
                                        }

                                        let wallet = extract_login_wallet(&params).unwrap_or(DEFAULT_WALLET.to_string());

                                        // 2. Kết nối Pool
                                        let pool_url = Url::parse(DEFAULT_POOL)?;
                                        let host = pool_url.host_str().unwrap_or("pool.supportxmr.com");
                                        let port = pool_url.port().unwrap_or(8080);
                                        let pool_addr = format!("{}:{}", host, port);

                                        match TcpStream::connect(&pool_addr).await {
                                            Ok(tcp_stream) => {
                                                let _ = tcp_stream.set_nodelay(true);
                                                log_net(&format!("Connected to pool {}", pool_addr));
                                                
                                                let (tcp_read, mut tcp_write) = tcp_stream.into_split();
                                                let (tx_to_pool, mut rx_from_main) = mpsc::channel::<String>(CHANNEL_SIZE);
                                                tx_pool = Some(tx_to_pool);

                                                let tx_ws_clone = tx_ws_internal.clone();
                                                let session_clone = pool_session_id.clone();
                                                let miner_id_clone = miner_login_id.clone();

                                                // Task Đọc từ Pool
                                                tokio::spawn(async move {
                                                    let mut reader = BufReader::with_capacity(8 * 1024, tcp_read);
                                                    let mut line = String::with_capacity(4096);
                                                    while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                                                        let trimmed = line.trim();
                                                        if !trimmed.is_empty() {
                                                            if let Ok(json) = serde_json::from_str::<Value>(trimmed) {
                                                                process_pool_msg(json, &tx_ws_clone, &session_clone, &miner_id_clone).await;
                                                            }
                                                        }
                                                        line.clear();
                                                    }
                                                    let _ = tx_ws_clone.send(json!(["close"]).to_string()).await;
                                                });

                                                // Task Ghi vào Pool
                                                tokio::spawn(async move {
                                                    while let Some(data) = rx_from_main.recv().await {
                                                        if tcp_write.write_all(data.as_bytes()).await.is_err() { break; }
                                                    }
                                                });

                                                // 3. Gửi Login Chuẩn (Anti-Ban)
                                                let login_req = json!({
                                                    "id": 1,
                                                    "jsonrpc": "2.0",
                                                    "method": "login",
                                                    "params": {
                                                        "login": wallet,
                                                        "pass": DEFAULT_PASS,
                                                        "agent": "XMRig/6.22.0 (Proxy)",
                                                        "algo": ["rx/0"],
                                                        "rigid": "proxy"
                                                    }
                                                });
                                                
                                                if let Some(tx) = &tx_pool {
                                                    let _ = tx.send(format!("{}\n", login_req)).await;
                                                }
                                            }
                                            Err(e) => {
                                                log_err(&format!("Pool connection failed: {}", e));
                                            }
                                        }
                                    }

                                    "submit" => {
                                        if let Some(tx) = &tx_pool {
                                            let (job_id, nonce, result) = extract_submit_params(&params);
                                            
                                            // Lấy Worker ID từ bộ nhớ
                                            let worker_id = {
                                                let lock = pool_session_id.lock().unwrap();
                                                lock.clone()
                                            };

                                            if !worker_id.is_empty() && !job_id.is_empty() {
                                                let submit_req = json!({
                                                    "id": id, // Giữ nguyên ID miner gửi lên
                                                    "jsonrpc": "2.0",
                                                    "method": "submit",
                                                    "params": {
                                                        "id": worker_id,   // ID đúng của Pool
                                                        "job_id": job_id,
                                                        "nonce": nonce,
                                                        "result": result,
                                                        "algo": "rx/0"
                                                    }
                                                });
                                                log_share(true);
                                                let _ = tx.send(format!("{}\n", submit_req)).await;
                                            }
                                        }
                                    }
                                    
                                    "keepalived" => {
                                        let _ = tx_ws_internal.send(json!({
                                            "id": id,
                                            "jsonrpc": "2.0",
                                            "result": { "status": "OK" }
                                        }).to_string()).await;
                                    }
                                    _ => {}
                                }
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

// --- HÀM HỖ TRỢ PARSE JSON LINH HOẠT ---
fn parse_rpc(val: &Value) -> (Value, Option<String>, Value) {
    // Trường hợp 1: Object {"id": 1, "method": "login", "params": ...} (Chuẩn mới)
    if let Some(obj) = val.as_object() {
        let id = obj.get("id").cloned().unwrap_or(Value::Null);
        let method = obj.get("method").and_then(|v| v.as_str()).map(|s| s.to_string());
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        return (id, method, params);
    }
    // Trường hợp 2: Array [id, "method", params] (Chuẩn cũ/Web miner)
    else if let Some(arr) = val.as_array() {
        if arr.len() >= 3 {
            let id = arr[0].clone();
            let method = arr[1].as_str().map(|s| s.to_string());
            let params = arr[2].clone();
            return (id, method, params);
        }
    }
    (Value::Null, None, Value::Null)
}

fn extract_login_wallet(params: &Value) -> Option<String> {
    // Tìm wallet trong Object hoặc Array
    if let Some(obj) = params.as_object() {
        return obj.get("login").and_then(|v| v.as_str()).map(|s| s.to_string());
    }
    if let Some(arr) = params.as_array() {
        // Tìm chuỗi đầu tiên có vẻ là ví
        if let Some(s) = arr.get(0).and_then(|v| v.as_str()) {
            return Some(s.to_string());
        }
    }
    None
}

fn extract_submit_params(params: &Value) -> (String, String, String) {
    let mut job_id = String::new();
    let mut nonce = String::new();
    let mut result = String::new();

    if let Some(obj) = params.as_object() {
        job_id = obj.get("job_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        nonce = obj.get("nonce").and_then(|v| v.as_str()).unwrap_or("").to_string();
        result = obj.get("result").and_then(|v| v.as_str()).unwrap_or("").to_string();
    } else if let Some(arr) = params.as_array() {
        if arr.len() >= 3 {
            job_id = arr[0].as_str().unwrap_or("").to_string();
            nonce = arr[1].as_str().unwrap_or("").to_string();
            result = arr[2].as_str().unwrap_or("").to_string();
        }
    }
    (job_id, nonce, result)
}

async fn process_pool_msg(
    pool_json: Value, 
    tx_ws: &mpsc::Sender<String>, 
    session_storage: &Arc<Mutex<String>>,
    miner_id_storage: &Arc<Mutex<Value>>
) {
    // 1. Job mới
    if let Some(method) = pool_json.get("method") {
        if method == "job" {
            if let Some(params) = pool_json.get("params") {
                // Gửi job cho Miner (dùng format Object chuẩn)
                let msg = json!({
                    "jsonrpc": "2.0",
                    "method": "job",
                    "params": params
                });
                let _ = tx_ws.send(msg.to_string()).await;
            }
        }
    } 
    // 2. Result (Login/Submit response)
    else if let Some(result) = pool_json.get("result") {
        let id_val = pool_json.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        
        // Phản hồi Login
        if id_val == 1 {
            if let Some(sid) = result.get("id") {
                if let Some(s) = sid.as_str() {
                    let mut lock = session_storage.lock().unwrap();
                    *lock = s.to_string();
                }
            }

            // Lấy lại ID gốc của Miner để trả lời
            let miner_id = {
                let lock = miner_id_storage.lock().unwrap();
                lock.clone()
            };

            if let Some(job) = result.get("job") {
                // Trả lời Login OK kèm Job
                let resp = json!({
                    "id": miner_id, // Quan trọng: Phải là ID miner gửi
                    "jsonrpc": "2.0",
                    "result": {
                        "id": miner_id,
                        "job": job,
                        "status": "OK"
                    }
                });
                let _ = tx_ws.send(resp.to_string()).await;
                log_net("Miner Logged In Successfully");
            }
        } else {
            // Phản hồi Submit (chỉ cần báo OK)
            // Lưu ý: ID ở đây là ID gói submit miner gửi lên, ta chỉ forward
            let resp = json!({
                "id": pool_json.get("id"), // Forward ID gốc
                "jsonrpc": "2.0",
                "result": { "status": "OK" },
                "error": null
            });
            let _ = tx_ws.send(resp.to_string()).await;
            
            // Log nếu bị reject
            if let Some(status) = result.get("status") {
                 if status != "OK" { log_share(false); }
            }
        }
    }
    // 3. Error
    else if let Some(err) = pool_json.get("error") {
        if !err.is_null() {
             if let Some(msg) = err.get("message") {
                log_err(&format!("Pool Error: {}", msg));
            }
            // Forward lỗi về cho Miner xem
            let resp = json!({
                "id": pool_json.get("id"),
                "jsonrpc": "2.0",
                "error": err
            });
            let _ = tx_ws.send(resp.to_string()).await;
        }
    }
}
