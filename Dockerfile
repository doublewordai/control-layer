# Backend build stage
FROM rust:1.88.0-slim AS backend-builder

# Install build dependencies including Node.js
RUN apt-get update && apt-get install -y \
  pkg-config \
  libssl-dev \
  curl \
  && curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
  && apt-get install -y nodejs \
  && rm -rf /var/lib/apt/lists/*

# Create app directory
WORKDIR /app

# Install cargo-watch for auto-reloading in dev
RUN cargo install cargo-watch

# Copy workspace and dwctl code
COPY Cargo.toml Cargo.lock ./
COPY dwctl/ dwctl/
COPY dashboard/ dashboard/

# Build frontend and copy to dwctl/static
WORKDIR /app/dashboard
RUN npm ci && npm run build
WORKDIR /app
RUN rm -rf dwctl/static && cp -r dashboard/dist dwctl/static

# Build from workspace root (target dir will be at /app/target)
# Use --no-default-features to disable embedded-db for Docker (uses external postgres)
ENV SQLX_OFFLINE=true
RUN cargo build --release -p dwctl --no-default-features

# Development stage
FROM backend-builder AS dev

# Change to dwctl directory for cargo watch
WORKDIR /app/dwctl

# Expose port
EXPOSE 3001

# Default command for development: doesn't rebuild the frontend on changes
CMD ["cargo", "watch", "-w", "src", "-x", "run --no-default-features"]

# Runtime stage
FROM ubuntu:24.04

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
  ca-certificates \
  curl \
  && rm -rf /var/lib/apt/lists/*

# Set working directory
WORKDIR /app

# Copy the binary from backend builder stage (frontend is already embedded in the binary)
COPY --from=backend-builder /app/target/release/dwctl /app/dwctl

# Expose port (app uses 3001 by default)
EXPOSE 3001

# Run the application
ENTRYPOINT ["./dwctl"]
CMD []
