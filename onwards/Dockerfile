# Build from the control-layer repository root so Onwards resolves the local
# workspace crates used by the application.
FROM rust:1.97.1-slim AS builder

RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY dwctl/ dwctl/
COPY fusillade/ fusillade/
COPY fusillade-core/ fusillade-core/
COPY fusillade-arsenal/ fusillade-arsenal/
COPY onwards/ onwards/

RUN cargo build --locked --release --package onwards

FROM ubuntu:26.04

RUN apt-get update && apt-get install -y ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/onwards /app/onwards
RUN chown -R ubuntu:ubuntu /app

USER ubuntu

EXPOSE 3000
ENTRYPOINT ["./onwards"]
CMD []
