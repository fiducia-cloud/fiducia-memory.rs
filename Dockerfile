# syntax=docker/dockerfile:1
FROM rust:1.97.0-slim-bookworm@sha256:cfbb0e0ef7a73e736386bfa346f1cb0503c6d162969dc9426fb37834f3f64c25 AS build
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && apt-get clean
WORKDIR /build
COPY . .
RUN cargo build --locked --release --bin fiducia-memory \
    && strip target/release/fiducia-memory

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:66aa873a4a14fb164aa01296058efd8253744606d72715e45acface073359faa
COPY --from=build --chown=65532:65532 /build/target/release/fiducia-memory /usr/local/bin/fiducia-memory
EXPOSE 8100
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/fiducia-memory"]
