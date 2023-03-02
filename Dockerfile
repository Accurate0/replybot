FROM rust:1.67.0 as builder

WORKDIR /usr/src/replybot

COPY . .
RUN cargo install --path .

FROM debian:buster-slim

RUN apt-get update
RUN apt-get install -y --no-install-recommends ca-certificates
RUN update-ca-certificates
RUN rm -rf /var/lib/apt/lists/*

WORKDIR /tmp
COPY --from=builder /usr/local/cargo/bin/replybot /usr/local/bin/replybot
COPY config.json .
CMD ["replybot"]
