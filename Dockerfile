# syntax=docker/dockerfile:1
FROM rust:1-slim-bookworm AS build
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && apt-get clean
WORKDIR /build
COPY . .
RUN cargo build --locked --release --bin fiducia-memory \
    && strip target/release/fiducia-memory

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build --chown=65532:65532 /build/target/release/fiducia-memory /usr/local/bin/fiducia-memory
EXPOSE 8090 8100
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/fiducia-memory"]
