# syntax=docker/dockerfile:1
FROM rust:1.95.0-slim-bookworm@sha256:d7482085ff5b415f84dba5647ae71606650bdef00db7aeb69f4b3d170c3e4082 AS build
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && apt-get clean
WORKDIR /build
COPY . .
RUN cargo build --locked --release --bin fiducia-memory \
    && strip target/release/fiducia-memory

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:ce0d66bc0f64aae46e6a03add867b07f42cc7b8799c949c2e898057b7f75a151
COPY --from=build --chown=65532:65532 /build/target/release/fiducia-memory /usr/local/bin/fiducia-memory
EXPOSE 8100
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/fiducia-memory"]
