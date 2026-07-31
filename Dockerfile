FROM rust:1.88-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --locked --release -p aimedia

FROM debian:bookworm-slim
RUN useradd --create-home --uid 10001 aimedia \
    && mkdir -p /run/aimedia \
    && chown aimedia:aimedia /run/aimedia
COPY --from=builder /src/target/release/aimedia /usr/local/bin/aimedia
USER 10001
ENTRYPOINT ["aimedia"]
