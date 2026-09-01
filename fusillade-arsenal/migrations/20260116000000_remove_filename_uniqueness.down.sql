-- migrations/20260116000000_remove_filename_uniqueness.down.sql
-- This migration cannot be safely reversed once duplicate filenames exist.
-- To re-add filename uniqueness, create a new forward migration that:
-- 1. Identifies and handles duplicate filenames
-- 2. Re-creates the unique constraints
-- This is intentionally a no-op.

DO $$
BEGIN
    RAISE NOTICE 'Migration 20260116000000 cannot be automatically reversed.';
    RAISE NOTICE 'Duplicate filenames may exist in the system.';
    RAISE NOTICE 'To restore uniqueness, create a new forward migration.';
END $$;