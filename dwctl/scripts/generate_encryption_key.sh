#!/bin/bash
# Generate a secure 256-bit (32 byte) encryption key for use with the application
# Usage: ./scripts/generate_encryption_key.sh

set -e

# Generate 32 random bytes and encode as base64
KEY=$(openssl rand -base64 32)

echo "Generated encryption key:"
echo ""
echo "ENCRYPTION_KEY=${KEY}"
echo ""
echo "Add this to your environment variables or .env file"
echo "Keep this key secure and never commit it to version control!"
