FROM rust:1.67.0 AS chef

RUN rustup target add x86_64-unknown-linux-musl

RUN cargo install cargo-chef
WORKDIR /app

FROM chef AS planner

COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder

RUN apt-get update
RUN apt-get install -y --no-install-recommends ca-certificates musl-tools
RUN update-ca-certificates
RUN rm -rf /var/lib/apt/lists/*
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json --target x86_64-unknown-linux-musl

COPY . .
RUN cargo build --release --bin replybot --target x86_64-unknown-linux-musl

FROM alpine:latest AS runtime
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/replybot /usr/local/bin
ENTRYPOINT ["/usr/local/bin/replybot"]
