# Backend build stage
FROM rust:1.90.0-slim AS backend-builder

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

# Copy workspace code (fusillade, dwctl, dashboard)
COPY Cargo.toml Cargo.lock ./
COPY .sqlx/ .sqlx/
COPY fusillade/ fusillade/
COPY dwctl/ dwctl/
COPY dashboard/ dashboard/

# Build frontend and copy to dwctl/static
WORKDIR /app/dashboard
RUN npm ci && npm run build
WORKDIR /app
RUN rm -rf dwctl/static && cp -r dashboard/dist dwctl/static

# Build from workspace root with embedded database (target dir will be at /app/target)
ENV SQLX_OFFLINE=true
RUN cargo build --release -p dwctl

# Development stage
FROM backend-builder AS dev

# Expose port
EXPOSE 3001

# Default command for development: doesn't rebuild the frontend on changes
CMD ["cargo", "watch", "-w", "dwctl/src", "-x", "run -p dwctl"]

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

# Copy the binary from backend builder stage (frontend is already embedded in the binary)
COPY --from=backend-builder /app/target/release/dwctl /app/dwctl

# Change ownership of the app directory to the ubuntu user
RUN chown -R ubuntu:ubuntu /app

# Switch to non-root user
USER ubuntu

# Expose port (app uses 3001 by default)
EXPOSE 3001

# Run the application
ENTRYPOINT ["./dwctl"]
CMD []
