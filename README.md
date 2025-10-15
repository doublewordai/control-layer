# Waycast

Waycast provides a single, high-performance interface for routing, managing,
and securing inference across model providers, users and deployments - both
open-source and proprietary.

- Seamlessly switch between models
- Turn any model (self-hosted or hosted) into a production-ready API with full
auth and user controls
- Centrally govern, monitor, and audit all inference activity

## Getting started

### Docker compose

```bash
wget https://raw.githubusercontent.com/doublewordai/waycast/refs/heads/main/docker-compose.yml
docker compose up -d
```

With docker compose installed, the preceding command will start the waycast stack.

Navigate to `http://localhost:3001` to get started.
