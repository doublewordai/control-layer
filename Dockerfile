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
RUN mkdir -p /app/clay

# Copy clay code and dependencies
COPY clay/Cargo.toml clay/Cargo.lock clay/
COPY clay/src clay/src
COPY clay/migrations clay/migrations
COPY clay/.sqlx clay/.sqlx

WORKDIR /app/clay/

# Build the application with offline mode for SQLx
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

# Copy the binary from backend builder stage
COPY --from=backend-builder /app/clay/target/release/clay /app/clay

# Copy migrations
COPY clay/migrations ./app/migrations

# Copy frontend assets from frontend builder stage
COPY --from=dashboard-builder /app/dist /app/static

# Set working directory
WORKDIR /app

# Expose port (app uses 3001 by default)
EXPOSE 3001

# Run the application
ENTRYPOINT ["./clay"]
CMD []
