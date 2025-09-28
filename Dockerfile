# Build stage
FROM rust:1.90-slim AS builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Create app directory
WORKDIR /app

# Copy dependency files
COPY Cargo.toml Cargo.lock ./

# Copy source code
COPY src/ ./src/

# Build the application in release mode
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Create a non-root user
RUN useradd -r -s /bin/false kelvinbot

# Create data directory
RUN mkdir -p /data && chown kelvinbot:kelvinbot /data

# Expose data directory as volume
VOLUME ["/data"]

# Copy the binary from builder stage
COPY --from=builder /app/target/release/kelvin-bot /usr/local/bin/kelvin-bot

# Set ownership and permissions
RUN chown root:root /usr/local/bin/kelvin-bot && \
    chmod 755 /usr/local/bin/kelvin-bot

# Switch to non-root user
USER kelvinbot

# Set working directory
WORKDIR /app

# Set default data directory
ENV KELVIN__DATA_DIRECTORY=/data

# Expose any ports if needed (currently none)
# EXPOSE 8080

# Health check (optional)
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD pgrep kelvin-bot || exit 1

# Run the application
CMD ["kelvin-bot"]