# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS builder
WORKDIR /app

# Pre-build dependencies for better BuildKit layer caching
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src templates \
    && echo "pub fn _stub() {}" > src/lib.rs \
    && echo "fn main() {}" > src/main.rs \
    && touch templates/dashboard.html \
    && cargo build --release --locked \
    && rm -rf src templates

# Build actual application binary
COPY src ./src
COPY templates ./templates
RUN touch src/lib.rs src/main.rs \
    && cargo build --release --locked \
    && strip target/release/ads-monitor

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home appuser

COPY --from=builder /app/target/release/ads-monitor /usr/local/bin/ads-monitor

USER appuser
EXPOSE 8000

LABEL org.opencontainers.image.source="https://github.com/picturous/ads-monitor"
LABEL org.opencontainers.image.description="Unified multi-platform Ads & Custom Audience Monitoring dashboard"

ENTRYPOINT ["ads-monitor"]
CMD ["--serve", "--host", "0.0.0.0", "--port", "8000", "--no-open"]
