ARG RUST_VERSION=1.79.0
ARG BINARY_NAME=reploybot

FROM rust:${RUST_VERSION}-slim-bookworm AS builder
ARG BINARY_NAME

RUN apt-get update -y && apt-get install -y pkg-config libssl-dev

WORKDIR /app/${BINARY_NAME}-build

COPY . .
RUN \
    --mount=type=cache,target=/app/${BINARY_NAME}-build/target/ \
    --mount=type=cache,target=/usr/local/cargo/registry/ \
    cargo build --locked --release --bin ${BINARY_NAME} && \
    cp ./target/release/${BINARY_NAME} /app

FROM debian:bookworm-slim AS final
ARG BINARY_NAME

RUN apt-get update -y && apt-get install -y libssl-dev ca-certificates
RUN update-ca-certificates
RUN groupadd -g 10001 appuser
RUN adduser \
    --disabled-password \
    --gecos "" \
    --home "/nonexistent" \
    --shell "/sbin/nologin" \
    --no-create-home \
    --uid "10001" \
    --gid "10001" \
    appuser

COPY --from=builder /app/${BINARY_NAME} /usr/local/bin/${BINARY_NAME}
RUN chown appuser /usr/local/bin/${BINARY_NAME}
RUN apt-get update && apt-get install -y curl

USER appuser

WORKDIR /opt/${BINARY_NAME}

RUN ln -s /usr/local/bin/${BINARY_NAME} executable
ENTRYPOINT ["./executable"]
HEALTHCHECK --interval=30s --timeout=30s --start-period=5s --retries=3 CMD [ "curl", "-f", "http://localhost:8000/health" ]
EXPOSE 8000/tcp
