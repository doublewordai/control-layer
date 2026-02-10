# Onwards Sync Notification System

## Overview

The onwards sync system keeps the AI proxy's routing cache in sync with database changes (API keys, user credits, model configurations). It uses two mechanisms:

1. **LISTEN/NOTIFY (primary)**: Fast, event-driven sync triggered by database changes
2. **Fallback timer (secondary)**: Periodic sync every N seconds for reliability

This document explains how the system works and how to configure it.

## Architecture

```
Database Changes → LISTEN/NOTIFY → Onwards Cache Reload
                     (primary)

Timer Tick ──────→ Periodic Sync → Onwards Cache Reload
              (fallback, every 10s)
```

### Event-Driven Sync (LISTEN/NOTIFY)

When database changes occur (API key created, credits deducted, etc.), PostgreSQL triggers send notifications:

1. Database change happens (e.g., `INSERT INTO api_keys`)
2. Trigger executes `pg_notify('auth_config_changed', payload)`
3. Onwards sync service receives notification via LISTEN
4. Cache reloads ALL routing data (API keys, user balances, model configs)

**Rate Limiting**: When users have depleted balances and continue making requests, the analytics batcher would trigger sync on every batch write. To prevent notification storms, we globally rate-limit these triggers (default: 5 seconds).

### Fallback Timer

Independent of notifications, a timer periodically syncs the cache:

- Runs every N seconds (default: 10s)
- Loads routing data directly from database
- Updates cache regardless of notification state

This ensures eventual consistency even if LISTEN/NOTIFY fails.

## Configuration

### Rate Limiting

Controls how frequently the batcher can trigger onwards sync for balance depletion:

```yaml
analytics:
  balance_notification_interval_seconds: 5
```

Environment variable: `DWCTL_ANALYTICS__BALANCE_NOTIFICATION_INTERVAL_SECONDS=5`

**How it works**: When processing a batch of credit transactions, if any users have balance ≤ 0, the batcher checks if we've notified in the last N seconds. If not, it sends ONE notification that triggers a full cache reload.

### Fallback Interval

Controls how often the fallback timer syncs:

```yaml
background_services:
  onwards_sync:
    enabled: true
    fallback_interval_seconds: 10
```

Environment variable: `DWCTL_BACKGROUND_SERVICES__ONWARDS_SYNC__FALLBACK_INTERVAL_SECONDS=10`

## Multi-Instance Behavior

In multi-instance deployments:

- **Rate limiting**: Each instance rate-limits independently. If 3 instances each send a notification at the rate limit (5s), the global rate is ~1.67s. This is acceptable since the cache reload is idempotent.
- **Fallback timer**: Each instance syncs independently every 10s. No leader election needed.

## Monitoring

Key metrics:

- `dwctl_onwards_sync_notifications_total{action="sent"}`: Notifications sent by batcher
- `dwctl_onwards_sync_notifications_total{action="rate_limited"}`: Notifications skipped due to rate limit
- `dwctl_cache_sync_total{source="listen_notify"}`: Syncs triggered by LISTEN/NOTIFY
- `dwctl_cache_sync_total{source="fallback"}`: Syncs triggered by fallback timer

If `rate_limited` is high relative to `sent`, users with depleted balances are making frequent requests but the cache only syncs once per interval (working as intended).

## Implementation Details

### Batcher Rate Limiting (`batcher.rs`)

```rust
// Global rate limiter (single timestamp, not per-user)
last_onwards_sync_notification: Arc<RwLock<Instant>>

// Check if enough time has passed
async fn should_notify_onwards_sync(&self) -> bool {
    let now = Instant::now();
    let mut last = self.last_onwards_sync_notification.write().await;

    if now.duration_since(*last) >= self.onwards_sync_notification_interval {
        *last = now;
        true
    } else {
        false
    }
}

// In batch processing:
let depleted_users: Vec<Uuid> = balances.iter()
    .filter(|(_, balance)| **balance <= Decimal::ZERO)
    .map(|(user_id, _)| *user_id)
    .collect();

if !depleted_users.is_empty() && self.should_notify_onwards_sync().await {
    // Send ONE notification (not per-user)
    self.notify_onwards_sync(&mut tx, &depleted_users).await?;
}
```

**Why global?** The onwards sync reloads ALL user data, so per-user rate limiting doesn't make sense. One notification triggers a full reload regardless of which user caused it.

### Onwards Sync Service (`onwards_config.rs`)

```rust
loop {
    tokio::select! {
        // Primary: LISTEN/NOTIFY
        Some(notification) = listener.recv() => {
            let targets = load_targets_from_db(&db).await?;
            sender.send(targets)?;
        }

        // Secondary: Fallback timer
        _ = fallback_timer.tick() => {
            let targets = load_targets_from_db(&db).await?;
            sender.send(targets)?;
        }
    }
}
```

Both paths do the same thing: load fresh data from database and send to onwards cache.

## Troubleshooting

### Cache is stale (API keys not working)

Check if onwards sync is running:
```bash
# Look for log: "Onwards config sync started"
docker logs dwctl | grep "Onwards config sync"
```

Check fallback timer is working:
```bash
# Should see periodic syncs every 10s
docker logs dwctl | grep "fallback sync"
```

### Too many cache syncs

If `dwctl_cache_sync_total` is very high:

1. Check notification rate limit: `analytics.balance_notification_interval_seconds`
2. Check fallback interval: `background_services.onwards_sync.fallback_interval_seconds`
3. Check if many users have balance ≤ 0 (notifications will trigger frequently but be rate-limited)

### Users with depleted balance can still make requests

This is expected for up to N seconds (rate limit + fallback interval). The sync happens eventually:

- Batcher triggers notification (rate-limited to 5s)
- Fallback syncs every 10s regardless
- Worst case: user makes requests for ~10s before cache updates

## Design Rationale

**Why global rate limiting?** The onwards sync is global (reloads ALL routing data), so per-user rate limiting would send redundant notifications. One notification is sufficient to reload everything.

**Why fallback timer?** LISTEN/NOTIFY can miss events (connection issues, notifications lost). The fallback ensures cache eventually syncs even if events are lost.

**Why in-memory rate limiting?** Adding database queries to the hot path (analytics batcher) would add latency and load. In-memory tracking is fast and works well enough in multi-instance setups.

**Why allow multi-instance redundancy?** Coordinating rate limiting across instances adds complexity and failure modes. Sending ~3x notifications is acceptable since cache reloads are idempotent and cheap.
