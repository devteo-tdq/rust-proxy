# --- Giai đoạn 1: Builder (Biên dịch) ---
FROM rust:1.83-slim-bookworm as builder

# Tạo thư mục làm việc
WORKDIR /app

# Copy file cấu hình trước để cache các thư viện (dependencies)
COPY Cargo.toml .

# Tạo một file main giả để tải và biên dịch các thư viện trước
# Giúp các lần build sau nhanh hơn nếu code thay đổi nhưng thư viện không đổi
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release

# Xóa file main giả và copy code thật vào
RUN rm src/main.rs
COPY src ./src

# Chạm vào file main để Cargo biết cần build lại code
RUN touch src/main.rs

# Biên dịch code thật ở chế độ tối ưu (Release)
RUN cargo build --release

# --- Giai đoạn 2: Runner (Chạy ứng dụng) ---
FROM debian:bookworm-slim

# Cài đặt chứng chỉ SSL (Cần thiết để kết nối HTTPS/WSS ra Pool)
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

# Thiết lập thư mục làm việc
WORKDIR /app

# Copy file chạy từ giai đoạn Builder sang
COPY --from=builder /app/target/release/rust_miner_proxy .

# Mở port 9000 (Port mà ứng dụng lắng nghe)
EXPOSE 9000

# Lệnh chạy ứng dụng
CMD ["./rust_miner_proxy"]
