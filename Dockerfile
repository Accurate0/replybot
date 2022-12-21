FROM rust:1.64.0 as builder
WORKDIR /usr/src/replybot
COPY . .
RUN cargo install --path .

FROM debian:buster-slim
RUN apt-get update && rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/local/cargo/bin/replybot /usr/local/bin/replybot
CMD ["replybot"]
