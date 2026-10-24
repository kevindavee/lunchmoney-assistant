# Build stage: compile the release binary.
FROM rust:1-bookworm AS builder
# cmake and clang build aws-lc (the TLS crypto library behind rustls).
RUN apt-get update && apt-get install -y --no-install-recommends cmake clang \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY build.rs diesel.toml ./
COPY migrations ./migrations
COPY src ./src
RUN cargo build --release --locked

# Runtime stage: the binary plus Node and the claude CLI (used from Phase 2).
FROM node:22-bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && npm install -g @anthropic-ai/claude-code \
    && npm cache clean --force
COPY --from=builder /app/target/release/expense-assistant /usr/local/bin/expense-assistant
# Run as the image's unprivileged user; /data is the Fly volume.
RUN mkdir -p /data && chown node:node /data
USER node
ENV PORT=8080
EXPOSE 8080
CMD ["expense-assistant"]
