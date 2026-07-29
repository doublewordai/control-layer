-- Add the capability role required to submit spare-capacity inference.
ALTER TYPE user_role ADD VALUE IF NOT EXISTS 'BACKGROUNDINFERENCEUSER';
