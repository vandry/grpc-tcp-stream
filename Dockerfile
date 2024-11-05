FROM alpine:latest
COPY target/x86_64-unknown-linux-musl/release/grpc-tcp-stream-receiver ./
