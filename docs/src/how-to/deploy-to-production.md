# Deploy to Production

> Checklist for deploying Control Layer to a production environment.

This guide covers the key changes needed when moving from a local setup to production.

## Security checklist

### 1. Change the admin password

The default config uses `hunter2`. Change it immediately:

```yaml
admin_email: "your-admin@company.com"
admin_password: "<strong-password>"
```

Or set via environment variables:

```bash
DWCTL_ADMIN_EMAIL=your-admin@company.com
DWCTL_ADMIN_PASSWORD=<strong-password>
```

### 2. Generate a secret key

The secret key signs JWT tokens. Generate a secure one:

```bash
openssl rand -base64 32
```

Set it in your environment:

```bash
SECRET_KEY=<your-generated-key>
```

### 3. Configure CORS

Add your production frontend URL to allowed origins:

```yaml
auth:
  security:
    cors:
      allowed_origins:
        - "https://your-app.company.com"
```

### 4. Use a production database

Point to your production PostgreSQL instance:

```bash
DATABASE_URL=postgres://user:password@your-db-host:5432/control_layer
```

## Infrastructure

### Run behind a reverse proxy

In production, run the Control Layer behind nginx, Caddy, or a cloud load balancer that handles:

- TLS termination
- Rate limiting
- Access logging

The Control Layer binds to `0.0.0.0:3001` by default. Your proxy should forward to this.

### Enable secure cookies

For HTTPS deployments, enable secure cookies:

```yaml
auth:
  native:
    session:
      cookie_secure: true
      cookie_same_site: "strict"
```

### Disable registration

Unless you want open signups, keep registration disabled:

```yaml
auth:
  native:
    allow_registration: false
```

Admins create users manually via the UI.

## Monitoring

Once deployed:

1. Set up health monitoring for your endpoints (see [Set Up Health Monitoring](health-monitoring.md))
2. Monitor the Control Layer's `/health` endpoint from your infrastructure
3. Set up log aggregation for request logs

## Quick reference

| Setting | Dev default | Production |
|---------|-------------|------------|
| `admin_password` | `hunter2` | Strong password |
| `secret_key` | None | Random 32+ bytes |
| `cookie_secure` | `true` | `true` |
| `allow_registration` | `false` | `false` |
| `cors.allowed_origins` | `localhost:3001` | Your domain |
