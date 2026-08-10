# syntax=docker/dockerfile:1
FROM rust:1.97.1-slim-bookworm@sha256:96c0af8cf054fd006435089f0076729716784ec9be485bd655de59c55df105ce AS build
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && apt-get clean
WORKDIR /build
COPY . .
RUN cargo build --locked --release --bin fiducia-memory \
    && strip target/release/fiducia-memory

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:fccdbb0a547c14e23fcf4ce8ad62ca5d43b4faae8d22cd292f490fef9946c96e
COPY --from=build --chown=65532:65532 /build/target/release/fiducia-memory /usr/local/bin/fiducia-memory
EXPOSE 8100
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/fiducia-memory"]
