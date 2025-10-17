-- Add initial tables for Zeus management server

-- Create user role enum
CREATE TYPE user_role AS ENUM ('ADMIN', 'USER', 'DEVELOPER', 'OBSERVER');

-- Users table
CREATE TABLE users (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    username VARCHAR NOT NULL UNIQUE,
    email VARCHAR NOT NULL UNIQUE,
    display_name VARCHAR,
    avatar_url VARCHAR,
    auth_source VARCHAR NOT NULL DEFAULT 'vouch',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_login TIMESTAMPTZ
);

-- User roles table (many-to-many)
CREATE TABLE user_roles (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role user_role NOT NULL,
    UNIQUE (user_id, role)
);

-- API Keys table
CREATE TABLE api_keys (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name VARCHAR NOT NULL,
    description TEXT,
    secret VARCHAR NOT NULL UNIQUE,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_used TIMESTAMPTZ
);

-- Inference Endpoints table
CREATE TABLE inference_endpoints (
    -- use integers for inference endpoints
    id SERIAL PRIMARY KEY,
  
    name VARCHAR NOT NULL UNIQUE,
    description TEXT,
    url VARCHAR NOT NULL,
    
    -- Ownership
    created_by UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    
    -- Metadata
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Deployed Models table
CREATE TABLE deployed_models (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    model_name VARCHAR NOT NULL,
    alias VARCHAR NOT NULL,
    description TEXT,
    
    -- Model type (chat, embed, etc.) - nullable
    type VARCHAR,
    -- Model capabilities as JSON array of strings - nullable
    capabilities TEXT[],
    
    -- Ownership
    created_by UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    hosted_on INTEGER REFERENCES inference_endpoints(id) ON DELETE CASCADE,
    
    -- Metadata
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Groups table
CREATE TABLE groups (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name VARCHAR NOT NULL UNIQUE,
    description TEXT,
    created_by UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- User-Group junction table  
CREATE TABLE user_groups (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    group_id UUID NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    joined_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (user_id, group_id)
);

-- Deployment-Group access table
CREATE TABLE deployment_groups (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    deployment_id UUID NOT NULL REFERENCES deployed_models(id) ON DELETE CASCADE,
    group_id UUID NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    granted_by UUID REFERENCES users(id),
    granted_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (deployment_id, group_id)
);


-- Indexes
CREATE INDEX idx_users_username ON users(username);
CREATE INDEX idx_users_email ON users(email);
CREATE INDEX idx_user_roles_user_id ON user_roles(user_id);
CREATE INDEX idx_api_keys_user_id ON api_keys(user_id);
CREATE INDEX idx_api_keys_secret ON api_keys(secret);
CREATE INDEX idx_deployed_models_model_name ON deployed_models(model_name);
CREATE INDEX idx_deployed_models_created_by ON deployed_models(created_by);

-- Group and access indexes
CREATE INDEX idx_user_groups_user_id ON user_groups(user_id);
CREATE INDEX idx_user_groups_group_id ON user_groups(group_id);
CREATE INDEX idx_deployment_groups_deployment_id ON deployment_groups(deployment_id);
CREATE INDEX idx_deployment_groups_group_id ON deployment_groups(group_id);

-- Insert system user with nil UUID only if it doesn't exist
INSERT INTO users (id, username, email, display_name, auth_source) 
SELECT '00000000-0000-0000-0000-000000000000', 'system', 'system@internal', 'System User', 'internal'
WHERE NOT EXISTS (SELECT 1 FROM users WHERE id = '00000000-0000-0000-0000-000000000000');

-- Insert system user role only if it doesn't exist
INSERT INTO user_roles (user_id, role) 
SELECT '00000000-0000-0000-0000-000000000000', 'ADMIN'
WHERE NOT EXISTS (SELECT 1 FROM user_roles WHERE user_id = '00000000-0000-0000-0000-000000000000' AND role = 'ADMIN');

-- Insert system API key with placeholder secret (will be updated on boot)
INSERT INTO api_keys (
    id,
    name, 
    description,
    secret,
    user_id,
    created_at,
    last_used
) 
SELECT 
    '00000000-0000-0000-0000-000000000000',
    'System AI Gateway Key',
    'System API key for internal AI gateway routing with access to all deployments',
    'sk-placeholder-will-be-updated-on-boot',
    '00000000-0000-0000-0000-000000000000',
    NOW(),
    NULL
WHERE NOT EXISTS (SELECT 1 FROM api_keys WHERE id = '00000000-0000-0000-0000-000000000000');

-- Insert Everyone group (automatic group that contains all users)
INSERT INTO groups (id, name, description, created_by, created_at, updated_at)
SELECT 
    '00000000-0000-0000-0000-000000000000',
    'Everyone',
    'Default group that automatically includes all users in the organization',
    '00000000-0000-0000-0000-000000000000',
    NOW(),
    NOW()
WHERE NOT EXISTS (SELECT 1 FROM groups WHERE id = '00000000-0000-0000-0000-000000000000');

