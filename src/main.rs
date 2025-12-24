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
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;
use url::Url;

// --- CẤU HÌNH ---
// Pool mặc định (SupportXMR)
const DEFAULT_POOL: &str = "stratum+tcp://pool.supportxmr.com:8080";
// Ví mặc định (Thay ví của bạn vào đây)
const DEFAULT_WALLET: &str = "44hQZfLkTccVGood4aYMTm1KPyJVoa9esLyq1bneAvhkchQdmFTx3rsD3KRwpXTUPd1iTF4VVGYsTCLYrxMZVsvtKqAmBiw";
// Tên Worker mặc định
const DEFAULT_PASS: &str = "ProxyWorker";
const LISTEN_PORT: u16 = 9000;
const CHANNEL_SIZE: usize = 2048; // Tăng buffer lên chút cho an toàn

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
    
    log_net(&format!("RANDOMX PROXY (Strict Protocol) listening on {}", LISTEN_PORT));
    
    while let Ok((stream, _)) = listener.accept().await {
        // Tắt Nagle Algorithm -> Giảm ping
        if let Err(e) = stream.set_nodelay(true) {
            eprintln!("Failed to set nodelay: {}", e);
        }

        tokio::spawn(async move {
            if let Err(e) = handle_client(stream).await {
                // Chỉ log lỗi nặng, bỏ qua lỗi ngắt kết nối thông thường
                if !e.to_string().contains("Closed") {
                    log_err(&format!("Client error: {}", e));
                }
            }
        });
    }
    Ok(())
}

async fn handle_client(stream: TcpStream) -> Result<()> {
    let callback = |req: &Request, response: Response| {
        log_net(&format!("New Client: {}", req.uri()));
        Ok(response)
    };

    let ws_stream = accept_hdr_async(stream, callback).await?;
    let (mut ws_sender, mut ws_receiver) = ws_stream.split();
    
    // Channel giao tiếp nội bộ
    let (tx_ws_internal, mut rx_ws_internal) = mpsc::channel::<String>(CHANNEL_SIZE);
    let mut tx_pool: Option<mpsc::Sender<String>> = None;

    // QUAN TRỌNG: Biến lưu Session ID từ Pool (Worker ID)
    let pool_session_id = Arc::new(Mutex::new(String::new()));

    loop {
        tokio::select! {
            // 1. Nhận tin từ Pool (qua channel nội bộ) -> Gửi xuống Miner
            Some(msg) = rx_ws_internal.recv() => {
                if ws_sender.send(Message::Text(msg)).await.is_err() {
                    break;
                }
            }

            // 2. Nhận tin từ Miner -> Xử lý -> Gửi lên Pool
            msg_opt = ws_receiver.next() => {
                match msg_opt {
                    Some(Ok(Message::Text(text))) => {
                        // Parse JSON mảng [id, method, params] hoặc Object
                        // Phần lớn Web Miner gửi dạng Array, XMRig gửi Object.
                        // Ở đây ta ưu tiên decode dạng phổ thông mà bạn đang dùng.
                        let parsed: Option<(Value, String, Value)> = serde_json::from_str(&text).ok();

                        if let Some((id, method, params)) = parsed {
                            match method.as_str() {
                                "login" => {
                                    // Lấy ví (nếu miner gửi lên), không thì dùng default
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
                                            tcp_stream.set_nodelay(true)?;
                                            log_net(&format!("Connected to pool {}", pool_addr));
                                            
                                            let (tcp_read, mut tcp_write) = tcp_stream.into_split();
                                            let (tx_to_pool_task, mut rx_from_main) = mpsc::channel::<String>(CHANNEL_SIZE);
                                            tx_pool = Some(tx_to_pool_task);

                                            // Task đọc dữ liệu từ Pool
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
                                                // Mất kết nối Pool -> Ngắt Miner để reset
                                                let _ = tx_ws_clone.send(json!(["close"]).to_string()).await;
                                            });

                                            // Task ghi dữ liệu vào Pool
                                            tokio::spawn(async move {
                                                while let Some(data) = rx_from_main.recv().await {
                                                    if tcp_write.write_all(data.as_bytes()).await.is_err() {
                                                        break;
                                                    }
                                                }
                                            });

                                            // --- GÓI TIN LOGIN CHUẨN RAMDOMX (CHỐNG BAN) ---
                                            let login_req = json!({
                                                "id": 1,
                                                "jsonrpc": "2.0",
                                                "method": "login",
                                                "params": {
                                                    "login": login_wallet,
                                                    "pass": DEFAULT_PASS,
                                                    // Giả danh XMRig bản mới nhất để Pool tin tưởng
                                                    "agent": "XMRig/6.22.0 (Proxy)",
                                                    // BẮT BUỘC: Khai báo rx/0 cho Monero/Salvium
                                                    "algo": ["rx/0"],
                                                    "rigid": "proxy"
                                                }
                                            });
                                            
                                            if let Some(tx) = &tx_pool {
                                                let _ = tx.send(format!("{}\n", login_req)).await;
                                            }
                                        }
                                        Err(e) => {
                                            log_err(&format!("Pool connection error: {}", e));
                                            let _ = tx_ws_internal.send(json!([id, "Pool Error", null]).to_string()).await;
                                        }
                                    }
                                }

                                "submit" => {
                                    if let Some(tx) = &tx_pool {
                                        if let Some(arr) = params.as_array() {
                                            if arr.len() >= 3 {
                                                // 1. Lấy Session ID (Worker ID) từ bộ nhớ
                                                let worker_id = {
                                                    let lock = pool_session_id.lock().unwrap();
                                                    lock.clone()
                                                };

                                                // Nếu chưa có ID (Login chưa xong) -> Không gửi để tránh bị Ban
                                                if !worker_id.is_empty() {
                                                    // 2. Tạo gói tin Submit chuẩn RandomX
                                                    let submit_req = json!({
                                                        "id": id, // ID RPC (tăng dần)
                                                        "jsonrpc": "2.0",
                                                        "method": "submit",
                                                        "params": {
                                                            "id": worker_id,  // ĐÚNG: Session ID Pool cấp
                                                            "job_id": arr[0], // ĐÚNG: Job ID của job hiện tại
                                                            "nonce": arr[1],  // Nonce
                                                            "result": arr[2], // Hash Result
                                                            "algo": "rx/0"    // Thêm cho chắc chắn (dù optional)
                                                        }
                                                    });
                                                    
                                                    log_share(true); // Log đẹp
                                                    let _ = tx.send(format!("{}\n", submit_req)).await;
                                                } else {
                                                    log_err("Submit ignored: No Session ID yet");
                                                }
                                            }
                                        }
                                    }
                                }
                                
                                "keepalived" => {
                                    // Trả lời Miner để nó không timeout
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
    // 1. Pool gửi việc mới (Method: job)
    if let Some(method) = pool_json.get("method") {
        if method == "job" {
            if let Some(params) = pool_json.get("params") {
                // Chuyển tiếp ngay lập tức cho Miner
                let _ = tx_ws.send(json!(["job", params]).to_string()).await;
            }
        }
    } 
    // 2. Pool trả lời kết quả (Result)
    else if let Some(result) = pool_json.get("result") {
        let id_val = pool_json.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        
        // ID = 1 là phản hồi của gói LOGIN
        if id_val == 1 {
            // LƯU SESSION ID (BẮT BUỘC ĐỂ KHÔNG BỊ DISCONNECT)
            if let Some(sid) = result.get("id") {
                if let Some(s) = sid.as_str() {
                    let mut lock = session_storage.lock().unwrap();
                    *lock = s.to_string();
                }
            }

            // Gửi Job đầu tiên cho Miner bắt đầu đào
            if let Some(job) = result.get("job") {
                let _ = tx_ws.send(json!([id_val, null, { "id": 0, "job": job }]).to_string()).await;
                log_net("Miner Logged In (RandomX Mode)");
            } else {
                let _ = tx_ws.send(json!([id_val, null, "OK"]).to_string()).await;
            }
        } 
        // ID khác 1 là phản hồi của gói SUBMIT
        else {
            // Kiểm tra xem share có bị reject không
            if let Some(status) = result.get("status") {
                 if status != "OK" {
                     log_share(false); // Log rejected
                 }
            }
            let _ = tx_ws.send(json!([id_val, null, "OK"]).to_string()).await;
        }
    } 
    // 3. Pool báo lỗi
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
