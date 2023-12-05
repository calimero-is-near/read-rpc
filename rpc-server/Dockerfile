FROM rust:1.69 AS builder
WORKDIR /tmp/

COPY Cargo.lock ./
RUN echo '[workspace]\nmembers = ["rpc-server", "database", "readnode-primitives"]' > Cargo.toml
COPY rpc-server/Cargo.toml rpc-server/Cargo.toml
COPY database database
COPY readnode-primitives readnode-primitives

# copy your source tree
COPY rpc-server/src ./rpc-server/src

# build for release
RUN cargo build --release --features send_tx_methods

FROM ubuntu:20.04
RUN apt update && apt install -yy openssl ca-certificates
COPY --from=builder /tmp/target/release/read-rpc-server .
EXPOSE 8080
ENTRYPOINT ["./read-rpc-server"]
