# Encryption Guide

This document explains how to use the encryption features in the Control Layer application.

## Overview

The application uses AES-256-GCM encryption to protect sensitive data at rest. The encryption key is provided via the `ENCRYPTION_KEY` environment variable.

## Setup

### 1. Generate an Encryption Key

Use the provided script to generate a secure 256-bit encryption key:

```bash
./scripts/generate_encryption_key.sh
```

This will output something like:

```
ENCRYPTION_KEY=YourBase64EncodedKeyHere==
```

### 2. Set the Environment Variable

Add the generated key to your environment:

```bash
export ENCRYPTION_KEY="YourBase64EncodedKeyHere=="
```

Or add it to your `.env` file (DO NOT commit this to version control):

```env
ENCRYPTION_KEY=YourBase64EncodedKeyHere==
```

**⚠️ IMPORTANT:** Keep this key secure! If you lose it, you will not be able to decrypt existing data.

## How It Works

### Encrypted Key Names

Instead of encrypting the *values* in the database, this system encrypts the *key names* themselves. This provides an additional layer of security:

- Without the correct `ENCRYPTION_KEY`, an attacker cannot even determine which keys exist in the database
- The key names are deterministically hashed using SHA256 with the encryption key
- The boolean values remain unencrypted in the `system_config` table

### Automatic Encryption Migration

When the application starts, it will:

1. Run all SQL migrations
2. Run the encryption migration (`migrate_to_encrypted_data()`) which:
   - Checks if encryption migration has already run (using an encrypted key name)
   - Creates an `encryption_key_set` marker with an encrypted key name
   - Marks the migration as complete
3. Continue with normal application startup

### Database Seeding

The `seed_database()` function stores a marker with an encrypted key name:

```rust
// The key name "encryption_key_set" is hashed with the encryption key
let encrypted_key_name = crypto::encrypt_key_name("encryption_key_set")?;
// Stored as: enc_<64_char_hex_hash> -> true
```

Without the `ENCRYPTION_KEY`, you cannot determine what "enc_abc123..." refers to.

## API Usage

### Encrypting Key Names

```rust
use crate::crypto;

// Generate an encrypted key name
let encrypted_key = crypto::encrypt_key_name("my_secret_config")?;
// Returns: "enc_<64_char_hex_hash>"

// Store in database with the encrypted key name
sqlx::query!(
    "INSERT INTO system_config (key, value) VALUES ($1, $2)",
    encrypted_key, true
)
.execute(pool)
.await?;

// Retrieve later using the same plaintext key name
let encrypted_key = crypto::encrypt_key_name("my_secret_config")?;
let value = sqlx::query_scalar!(
    "SELECT value FROM system_config WHERE key = $1",
    encrypted_key
)
.fetch_one(pool)
.await?;
```

### Encrypting Data Values (if needed)

You can also encrypt the values themselves:

```rust
use crate::crypto;

let plaintext = b"sensitive data";
let encrypted = crypto::encrypt_with_env_key(plaintext)?;
// encrypted is a base64-encoded string containing: nonce + ciphertext

// Decrypt later
let decrypted = crypto::decrypt_with_env_key(&encrypted)?;
// decrypted is a Vec<u8> containing the original plaintext
```

## Adding Encryption to Other Tables

To encrypt existing data in other tables, modify the `migrate_to_encrypted_data()` function in `src/main.rs`:

```rust
// Example: Encrypt API keys
let api_keys = sqlx::query!("SELECT id, secret FROM api_keys WHERE is_encrypted = false")
    .fetch_all(&mut *tx)
    .await?;

for key in api_keys {
    let encrypted_secret = crypto::encrypt_with_env_key(key.secret.as_bytes())?;
    sqlx::query!(
        "UPDATE api_keys SET secret = $1, is_encrypted = true WHERE id = $2",
        encrypted_secret, key.id
    )
    .execute(&mut *tx)
    .await?;
}
```

You may also need to add a migration to add an `is_encrypted` boolean column to track which records have been encrypted.

## Security Notes

1. **Key Name Encryption** uses SHA256 hashing with the encryption key for deterministic, secure key derivation
2. **AES-256-GCM** provides authenticated encryption (both confidentiality and integrity) for values
3. Each encryption operation uses a **random 96-bit nonce** (stored with the ciphertext)
4. The encryption key must be **32 bytes (256 bits)** when decoded from base64
5. **Never commit** the `ENCRYPTION_KEY` to version control
6. Without the encryption key, an attacker cannot:
   - Determine what configuration keys exist
   - Access any encrypted values
   - Correlate encrypted key names across different deployments
7. **Rotate keys** periodically by:
   - Migrating all encrypted key names to new hashes
   - Re-encrypting all data with the new key

## Troubleshooting

### Error: "ENCRYPTION_KEY environment variable not set"

Make sure the `ENCRYPTION_KEY` environment variable is set before starting the application.

### Error: "ENCRYPTION_KEY must be 32 bytes"

The key must be exactly 32 bytes when decoded from base64. Use the provided script to generate a valid key:

```bash
./scripts/generate_encryption_key.sh
```

### Error: "Decryption failed"

This usually means:
- The encryption key has changed
- The encrypted data has been corrupted
- The encrypted data was not created with `encrypt_with_env_key()`

## Key Rotation

To rotate the encryption key:

1. Backup your database
2. Create a temporary migration script that:
   - Decrypts all data with `OLD_ENCRYPTION_KEY`
   - Re-encrypts with `NEW_ENCRYPTION_KEY`
3. Test thoroughly before deploying to production
