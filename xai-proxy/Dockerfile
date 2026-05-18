FROM rust:1.86-slim AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/xai-proxy /usr/local/bin/
ENTRYPOINT ["xai-proxy"]
CMD ["serve", "--bind", "0.0.0.0"]
