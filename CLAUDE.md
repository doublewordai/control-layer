# Control Layer Project Context

## Project Structure

- **Backend**: [dwctl/](dwctl/) - Rust-based control layer service
- **Frontend**: [dashboard/](dashboard/) - React/TypeScript dashboard
- **Database**: PostgreSQL 15

### Services

1. **postgres** - Database on port 5433 (host) / 5432 (container)
   - DB: `control_layer`
   - User: `control_layer`
   - Password: `control_layer_password`

2. **control-layer** - Backend on port 3001
   - Live reload enabled via volume mounts
   - Built from Dockerfile (dev target)
   - Secret key: `mysupersecretkey`

3. **control-layer-frontend** - Frontend on port 5173
   - Vite dev server with HMR
   - Proxies to backend at `http://control-layer:3001`

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Development Commands

### Environment Setup

```bash
# Setup development environment (requires authentication with doublewordai GCP project)
just setup          # Generate self-signed certificates and decrypt environment
```

**Note**: To decrypt the `env.enc` file, you must be authenticated with the doublewordai GCP project:

```bash
gcloud auth login
```

### Running the System

Don't run these! the user should run them separately when prompted, and then we
should use `docker compose logs` to query their output

```bash
just up docker      # Start all services in 'prod' mode
just dev            # Development mode with hot reload. 
```

### Frontend Development

Test the frontend with:

```bash
just test ts # Run TypeScript tests. Args are passed to vitest, but we default to run mode (e.g. add --watch, for a live watcher)
```

```bash
just lint ts # test formatting, linting & typescript compilation. Args are passed to eslint.

just fmt ts # prettier
```

### Rust Services

RUn the unit tests for the rust backend:

```bash
just test rust
```

Linting & formatting:

```bash
just lint rust # fmt check + clippy (args get passed to clippy)

just fmt rust # actually formats
```

## Commit Message Guidelines

Use conventional commits format for all commit messages:

```
<type>(<scope>): <description>

[optional body]
```

**Types:**

- `feat`: New feature
- `fix`: Bug fix
- `docs`: Documentation changes
- `style`: Code style changes (formatting, etc.)
- `refactor`: Code refactoring
- `test`: Adding or updating tests
- `chore`: Build process or auxiliary tool changes
- `perf`: Performance improvements
- `ci`: CI/CD changes

**Examples:**

```
feat(dashboard): add user profile management
fix(clay): resolve database connection timeout
docs: update setup instructions for sops usage
refactor(api): simplify user authentication flow
```

**Important:**

- Keep the description concise and focused on what changed
- Use present tense ("add" not "added")
- Don't include co-authoring unless explicitly requested
- Describe the diff, not the process of making changes
