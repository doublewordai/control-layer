-- Analytics table for pre-computed request/response metrics
-- This eliminates the need for expensive JSON parsing in aggregation queries

CREATE TABLE http_analytics (
    id BIGSERIAL PRIMARY KEY,
    instance_id UUID NOT NULL,
    correlation_id BIGINT NOT NULL,
    timestamp TIMESTAMP WITH TIME ZONE NOT NULL,
    
    -- Request fields
    method VARCHAR(10) NOT NULL,
    uri TEXT NOT NULL,
    model TEXT,
    
    -- Response fields  
    status_code INTEGER,
    duration_ms BIGINT,
    
    -- Token metrics (extracted from JSON)
    prompt_tokens BIGINT DEFAULT 0,
    completion_tokens BIGINT DEFAULT 0,
    total_tokens BIGINT DEFAULT 0,
    
    -- Response type for different handling
    response_type TEXT, -- 'chat_completion', 'embedding', 'streaming', etc.
    
    created_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT NOW(),
    
    -- Foreign key relationship to original tables
    UNIQUE(instance_id, correlation_id)
);

-- Indexes for analytics queries
CREATE INDEX idx_analytics_timestamp ON http_analytics (timestamp DESC);
CREATE INDEX idx_analytics_model_timestamp ON http_analytics (model, timestamp DESC) WHERE model IS NOT NULL;
CREATE INDEX idx_analytics_uri_timestamp ON http_analytics (uri, timestamp DESC) WHERE uri LIKE '/ai/%';
CREATE INDEX idx_analytics_status_timestamp ON http_analytics (status_code, timestamp DESC) WHERE status_code IS NOT NULL;

-- Composite index for common analytics queries
CREATE INDEX idx_analytics_full_query ON http_analytics (
    timestamp DESC, 
    model, 
    status_code, 
    prompt_tokens, 
    completion_tokens, 
    duration_ms
) WHERE uri LIKE '/ai/%';