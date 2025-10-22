# Probe System TODO

## Probe routing

11. Currently the probes go straight to the upstream endpoints, but they should
    actually be routed through the control layer - via the alias. That way they
get logged, budgeted, etc.

## Architecture & Code Quality

12. **Refactor probe code integration**

- Review and align probe implementation with existing codebase patterns
- Should be more layered design than domain driven - i.e. the probes thing should be in the repository pattern? We could alternatively do a big refactor into domains - i.e. folders + routers for users-groups/, api-keys/, endpoints/, models/, etc, but this might be too big.
- Ensure consistency with current architecture and conventions
- Should be doing compile time checked queries in sqlx.
- The REST API for probes is bad, and we should redesign it.

## Deal with terrible leader election stuff

13. Currently we do leader election to figure out which replica runs the probes.
    But we're not at all careful about it - i.e. if a leader scales down, then
there's just no leader, since the other replicas don't try to take the lock
after startup. We should do it right. If there's any better solution than this
(i.e. putting run_probes in a toggle, and then setting that toggle in only one
replica when deployed via k8s), that would be better than bad distributed
systems logic.

## Testing

11. Gotta do some testing, both backend and frontend.
