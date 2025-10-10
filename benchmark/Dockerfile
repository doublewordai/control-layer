
# --- Build Stage ---
FROM rust:latest as builder

# Create a new empty shell project
WORKDIR /usr/src/app
COPY . .

# Build the release binary
RUN cargo build --release

# --- Final Stage ---
FROM gcr.io/distroless/cc-debian12

# Copy the compiled binary from the build stage
COPY --from=builder /usr/src/app/target/release/openai-benchmark-rust /usr/local/bin/openai-benchmark-rust

# Expose the port the app runs on
EXPOSE 8080

# Set the entrypoint
ENTRYPOINT ["/usr/local/bin/openai-benchmark-rust"]
