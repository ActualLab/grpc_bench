FROM rust:1.100 AS builder

WORKDIR /app
COPY tonic_client /app
COPY proto /app/proto

RUN apt-get update && apt-get install -y cmake protobuf-compiler
RUN cargo build --release --locked

FROM debian:stable-slim
COPY --from=builder /app/target/release/tonic_client /usr/local/bin/tonic_client
ENTRYPOINT ["tonic_client"]
