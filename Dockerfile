# Dependency planner stage
FROM lukemathwalker/cargo-chef:latest-rust-1.93.0-slim AS chef
WORKDIR /app

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY dwctl/ dwctl/
RUN cargo chef prepare --recipe-path recipe.json

# Backend build stage
FROM chef AS builder

# Install build dependencies including Node.js
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    curl \
    && curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
    && apt-get install -y nodejs \
    && rm -rf /var/lib/apt/lists/*

# Build dependencies (cached unless Cargo.toml/Cargo.lock change)
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

# Build frontend
COPY dashboard/ dashboard/
WORKDIR /app/dashboard
RUN npm ci && npm run build
WORKDIR /app

# Copy source and build
COPY .sqlx/ .sqlx/
COPY Cargo.toml Cargo.lock ./
COPY dwctl/ dwctl/
RUN rm -rf dwctl/static && cp -r dashboard/dist dwctl/static
ENV SQLX_OFFLINE=true
RUN cargo build --release -p dwctl

# Runtime stage
FROM ubuntu:24.04

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    libxml2 \
    tzdata \
    && rm -rf /var/lib/apt/lists/*

# Set working directory
WORKDIR /app

# Copy the binary from builder stage (frontend is already embedded in the binary)
COPY --from=builder /app/target/release/dwctl /app/dwctl

# Change ownership of the app directory to the ubuntu user
RUN chown -R ubuntu:ubuntu /app

# Switch to non-root user
USER ubuntu

# Expose port (app uses 3001 by default)
EXPOSE 3001

# Run the application
ENTRYPOINT ["./dwctl"]
CMD []
