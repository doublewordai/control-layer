-- Add public visibility flag to probes
ALTER TABLE probes ADD COLUMN public BOOLEAN NOT NULL DEFAULT false;

-- Create index for efficient querying of public probes
CREATE INDEX IF NOT EXISTS idx_probes_public ON probes(public) WHERE public = true;
