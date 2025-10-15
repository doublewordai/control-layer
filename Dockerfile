# Frontend build stage
FROM node:20-alpine AS frontend-builder

# Create app directory
WORKDIR /app

FROM frontend-builder AS dashboard-builder
# Copy package files
COPY dashboard/package*.json ./
RUN npm ci

# Copy frontend source
COPY dashboard/ ./
RUN npm run build

# Backend build stage
FROM rust:1.88.0-slim AS backend-builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
  pkg-config \
  libssl-dev \
  && rm -rf /var/lib/apt/lists/*

# Create app directory
WORKDIR /app
RUN mkdir -p /app/waycast

# Copy waycast code and dependencies
COPY waycast/Cargo.toml waycast/Cargo.lock waycast/
COPY waycast/src waycast/src
COPY waycast/migrations waycast/migrations
COPY waycast/.sqlx waycast/.sqlx

# Copy frontend build from dashboard-builder stage into static/ folder
# This will be embedded into the binary by rust-embed
COPY --from=dashboard-builder /app/dist waycast/static/

WORKDIR /app/waycast/

# Build the application with offline mode for SQLx
# The frontend assets in static/ will be embedded at compile time
ENV SQLX_OFFLINE=true
RUN cargo build --release

# Development stage
FROM backend-builder AS dev

# Install cargo-watch for auto-reloading
RUN cargo install cargo-watch

# Expose port
EXPOSE 3001

# Default command for development
CMD ["cargo", "watch", "-w", "src", "-x", "run"]

# Runtime stage
FROM ubuntu:24.04

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
  ca-certificates \
  curl \
  && rm -rf /var/lib/apt/lists/*

# Copy the binary from backend builder stage (frontend is already embedded in the binary)
COPY --from=backend-builder /app/waycast/target/release/waycast /app/waycast

# Copy migrations
COPY waycast/migrations ./app/migrations

# Set working directory
WORKDIR /app

# Expose port (app uses 3001 by default)
EXPOSE 3001

# Run the application
ENTRYPOINT ["./waycast"]
CMD []
