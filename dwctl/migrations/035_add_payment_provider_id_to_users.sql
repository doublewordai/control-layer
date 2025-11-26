-- Add payment_provider_id field to users table
-- This will store the customer ID from the payment provider (e.g., Stripe customer ID)
ALTER TABLE users ADD COLUMN payment_provider_id VARCHAR;

-- Add index for faster lookups
CREATE INDEX idx_users_payment_provider_id ON users(payment_provider_id);