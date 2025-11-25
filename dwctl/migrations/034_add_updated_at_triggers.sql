-- Create a generic function to update updated_at timestamp
-- This can be reused across multiple tables
CREATE OR REPLACE FUNCTION update_updated_at_column()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$ language 'plpgsql';

-- Add trigger for groups table
CREATE TRIGGER update_groups_updated_at
    BEFORE UPDATE ON groups
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();

-- Add trigger for inference_endpoints table
CREATE TRIGGER update_inference_endpoints_updated_at
    BEFORE UPDATE ON inference_endpoints
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();
