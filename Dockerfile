# Multi-stage Dockerfile for a3s-box agent
# Builds the agent, guest-init, and nsexec binaries and packages them into a minimal image

# =============================================================================
# Stage 1: Build guest-init and nsexec
# =============================================================================
FROM rust:1.75-alpine AS guest-builder

# Install build dependencies
RUN apk add --no-cache \
    musl-dev \
    protobuf-dev \
    protoc

# Add musl target
RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /build

# Create a minimal workspace for guest-init
RUN mkdir -p src

# Copy only the crates needed for guest-init
COPY src/core src/core
COPY src/guest src/guest

# Create a minimal workspace Cargo.toml
RUN echo '[workspace]' > src/Cargo.toml && \
    echo 'members = ["core", "guest/init"]' >> src/Cargo.toml && \
    echo 'resolver = "2"' >> src/Cargo.toml && \
    echo '' >> src/Cargo.toml && \
    echo '[workspace.package]' >> src/Cargo.toml && \
    echo 'version = "0.1.0"' >> src/Cargo.toml && \
    echo 'edition = "2021"' >> src/Cargo.toml && \
    echo 'authors = ["A3S Lab Team"]' >> src/Cargo.toml && \
    echo 'license = "MIT"' >> src/Cargo.toml

# Build guest-init and nsexec
RUN cd src && \
    cargo build --release \
    -p a3s-box-guest-init \
    --target x86_64-unknown-linux-musl

# =============================================================================
# Stage 2: Build code agent
# =============================================================================
FROM rust:1.75-alpine AS code-builder

# Install build dependencies
RUN apk add --no-cache \
    musl-dev \
    protobuf-dev \
    protoc

# Add musl target
RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /build

# Copy proto files and code agent
COPY src/proto src/proto
COPY src/code src/code

# Build code agent
RUN cd src/code && \
    cargo build --release \
    --target x86_64-unknown-linux-musl

# =============================================================================
# Stage 3: Runtime image
# =============================================================================
FROM alpine:3.19

# Install runtime dependencies
RUN apk add --no-cache \
    ca-certificates \
    libgcc

# Create necessary directories
RUN mkdir -p /a3s/workspace /sbin /usr/bin

# Copy binaries from builders
COPY --from=guest-builder /build/src/target/x86_64-unknown-linux-musl/release/a3s-box-guest-init /sbin/init
COPY --from=guest-builder /build/src/target/x86_64-unknown-linux-musl/release/a3s-box-nsexec /usr/bin/nsexec
COPY --from=code-builder /build/src/code/target/x86_64-unknown-linux-musl/release/a3s-code /usr/bin/a3s-code

# Set permissions
RUN chmod +x /usr/bin/a3s-code /sbin/init /usr/bin/nsexec

# Set working directory
WORKDIR /a3s/workspace

# Environment variables
ENV RUST_LOG=info
ENV WORKSPACE=/a3s/workspace

# =============================================================================
# OCI Image Labels
# =============================================================================

# Standard OCI labels
LABEL org.opencontainers.image.title="a3s-box-agent"
LABEL org.opencontainers.image.description="A3S Box coding agent with namespace isolation"
LABEL org.opencontainers.image.vendor="A3S Lab"
LABEL org.opencontainers.image.version="0.1.0"
LABEL org.opencontainers.image.source="https://github.com/A3S-Lab/Box"

# A3S Box agent configuration labels
# These labels are parsed by the runtime to configure the agent
LABEL a3s.box.agent.type="code"
LABEL a3s.box.agent.version="0.1.0"
LABEL a3s.box.agent.binary="/usr/bin/a3s-code"

# Default LLM configuration (can be overridden at runtime)
# LABEL a3s.box.llm.provider="anthropic"
# LABEL a3s.box.llm.model="claude-sonnet-4-20250514"

# Default entrypoint is the init process
# The init process will spawn the agent in an isolated namespace
ENTRYPOINT ["/sbin/init"]
