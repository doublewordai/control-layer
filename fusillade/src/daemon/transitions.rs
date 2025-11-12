//! State transition implementations for daemon lifecycle.

use super::types::{DaemonRecord, DaemonStats, Dead, Initializing, Running};
use crate::error::Result;
use crate::manager::DaemonStorage;

impl DaemonRecord<Initializing> {
    /// Transition daemon from Initializing to Running.
    pub async fn start<S: DaemonStorage + ?Sized>(
        self,
        storage: &S,
    ) -> Result<DaemonRecord<Running>> {
        let now = chrono::Utc::now();
        let record = DaemonRecord {
            data: self.data,
            state: Running {
                started_at: self.state.started_at,
                last_heartbeat: now,
                stats: DaemonStats::default(),
            },
        };
        storage.persist_daemon(&record).await?;
        Ok(record)
    }
}

impl DaemonRecord<Running> {
    /// Update heartbeat and stats.
    ///
    /// This should be called periodically from the daemon loop to indicate
    /// the daemon is still alive and to update processing statistics.
    pub async fn heartbeat<S: DaemonStorage + ?Sized>(
        self,
        stats: DaemonStats,
        storage: &S,
    ) -> Result<DaemonRecord<Running>> {
        let record = DaemonRecord {
            data: self.data,
            state: Running {
                started_at: self.state.started_at,
                last_heartbeat: chrono::Utc::now(),
                stats,
            },
        };
        storage.persist_daemon(&record).await?;
        Ok(record)
    }

    /// Transition daemon to Dead state (graceful shutdown).
    pub async fn shutdown<S: DaemonStorage + ?Sized>(
        self,
        storage: &S,
    ) -> Result<DaemonRecord<Dead>> {
        let record = DaemonRecord {
            data: self.data,
            state: Dead {
                started_at: self.state.started_at,
                stopped_at: chrono::Utc::now(),
                final_stats: self.state.stats,
            },
        };
        storage.persist_daemon(&record).await?;
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::{AnyDaemonRecord, DaemonData, DaemonStatus};
    use super::*;
    use crate::request::DaemonId;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    /// Mock storage for testing daemon state transitions.
    #[derive(Default, Clone)]
    struct MockDaemonStorage {
        records: Arc<Mutex<Vec<AnyDaemonRecord>>>,
    }

    #[async_trait]
    impl DaemonStorage for MockDaemonStorage {
        async fn persist_daemon<T: super::super::types::DaemonState + Clone>(
            &self,
            record: &DaemonRecord<T>,
        ) -> Result<()>
        where
            AnyDaemonRecord: From<DaemonRecord<T>>,
        {
            let any = AnyDaemonRecord::from(record.clone());
            let mut records = self.records.lock().unwrap();

            // Find and update or insert
            if let Some(existing) = records.iter_mut().find(|r| r.id() == any.id()) {
                *existing = any;
            } else {
                records.push(any);
            }

            Ok(())
        }

        async fn get_daemon(&self, daemon_id: DaemonId) -> Result<AnyDaemonRecord> {
            let records = self.records.lock().unwrap();
            records
                .iter()
                .find(|r| r.id() == daemon_id)
                .cloned()
                .ok_or_else(|| {
                    crate::error::FusilladeError::Other(anyhow::anyhow!("Daemon not found"))
                })
        }

        async fn list_daemons(
            &self,
            status_filter: Option<DaemonStatus>,
        ) -> Result<Vec<AnyDaemonRecord>> {
            let records = self.records.lock().unwrap();
            Ok(records
                .iter()
                .filter(|r| status_filter.is_none() || status_filter == Some(r.status()))
                .cloned()
                .collect())
        }
    }

    fn test_daemon_data() -> DaemonData {
        DaemonData {
            id: DaemonId(Uuid::new_v4()),
            hostname: "test-host".to_string(),
            pid: 12345,
            version: "test-v1".to_string(),
            config_snapshot: serde_json::json!({"test": "config"}),
        }
    }

    #[tokio::test]
    async fn test_initializing_to_running() {
        let storage = MockDaemonStorage::default();
        let data = test_daemon_data();
        let daemon_id = data.id;

        let initializing = DaemonRecord {
            data,
            state: Initializing {
                started_at: chrono::Utc::now(),
            },
        };

        // Transition to running
        let running = initializing.start(&storage).await.unwrap();

        assert_eq!(running.data.id, daemon_id);
        assert!(
            running.state.last_heartbeat > running.state.started_at - chrono::Duration::seconds(1)
        );

        // Verify it was persisted
        let retrieved = storage.get_daemon(daemon_id).await.unwrap();
        match retrieved {
            AnyDaemonRecord::Running(r) => {
                assert_eq!(r.data.id, daemon_id);
            }
            _ => panic!("Expected Running state"),
        }
    }

    #[tokio::test]
    async fn test_running_heartbeat() {
        let storage = MockDaemonStorage::default();
        let data = test_daemon_data();
        let daemon_id = data.id;

        let running = DaemonRecord {
            data,
            state: Running {
                started_at: chrono::Utc::now() - chrono::Duration::seconds(30),
                last_heartbeat: chrono::Utc::now() - chrono::Duration::seconds(10),
                stats: DaemonStats {
                    requests_processed: 10,
                    requests_failed: 2,
                    requests_in_flight: 5,
                },
            },
        };

        let old_heartbeat = running.state.last_heartbeat;

        // Send heartbeat with updated stats
        let updated_stats = DaemonStats {
            requests_processed: 15,
            requests_failed: 3,
            requests_in_flight: 3,
        };

        let updated = running.heartbeat(updated_stats, &storage).await.unwrap();

        assert_eq!(updated.data.id, daemon_id);
        assert!(updated.state.last_heartbeat > old_heartbeat);
        assert_eq!(updated.state.stats.requests_processed, 15);
        assert_eq!(updated.state.stats.requests_failed, 3);
        assert_eq!(updated.state.stats.requests_in_flight, 3);

        // Verify it was persisted
        let retrieved = storage.get_daemon(daemon_id).await.unwrap();
        match retrieved {
            AnyDaemonRecord::Running(r) => {
                assert_eq!(r.state.stats.requests_processed, 15);
            }
            _ => panic!("Expected Running state"),
        }
    }

    #[tokio::test]
    async fn test_running_to_dead() {
        let storage = MockDaemonStorage::default();
        let data = test_daemon_data();
        let daemon_id = data.id;

        let final_stats = DaemonStats {
            requests_processed: 100,
            requests_failed: 5,
            requests_in_flight: 0,
        };

        let running = DaemonRecord {
            data,
            state: Running {
                started_at: chrono::Utc::now() - chrono::Duration::minutes(10),
                last_heartbeat: chrono::Utc::now(),
                stats: final_stats.clone(),
            },
        };

        let started_at = running.state.started_at;

        // Shutdown
        let dead = running.shutdown(&storage).await.unwrap();

        assert_eq!(dead.data.id, daemon_id);
        assert_eq!(dead.state.started_at, started_at);
        assert!(dead.state.stopped_at > started_at);
        assert_eq!(dead.state.final_stats.requests_processed, 100);
        assert_eq!(dead.state.final_stats.requests_failed, 5);
        assert_eq!(dead.state.final_stats.requests_in_flight, 0);

        // Verify it was persisted
        let retrieved = storage.get_daemon(daemon_id).await.unwrap();
        match retrieved {
            AnyDaemonRecord::Dead(d) => {
                assert_eq!(d.state.final_stats.requests_processed, 100);
            }
            _ => panic!("Expected Dead state"),
        }
    }

    #[tokio::test]
    async fn test_full_lifecycle() {
        let storage = MockDaemonStorage::default();
        let data = test_daemon_data();
        let daemon_id = data.id;

        // Start: Initializing -> Running
        let initializing = DaemonRecord {
            data,
            state: Initializing {
                started_at: chrono::Utc::now(),
            },
        };

        let mut running = initializing.start(&storage).await.unwrap();

        // Heartbeats
        for i in 1..=5 {
            let stats = DaemonStats {
                requests_processed: i * 10,
                requests_failed: i,
                requests_in_flight: i as usize,
            };
            running = running.heartbeat(stats, &storage).await.unwrap();
        }

        assert_eq!(running.state.stats.requests_processed, 50);
        assert_eq!(running.state.stats.requests_failed, 5);

        // Shutdown: Running -> Dead
        let dead = running.shutdown(&storage).await.unwrap();

        assert_eq!(dead.data.id, daemon_id);
        assert_eq!(dead.state.final_stats.requests_processed, 50);

        // Verify final state in storage
        let retrieved = storage.get_daemon(daemon_id).await.unwrap();
        assert!(retrieved.is_terminal());
        assert!(matches!(retrieved, AnyDaemonRecord::Dead(_)));
    }

    #[tokio::test]
    async fn test_list_daemons_filtering() {
        let storage = MockDaemonStorage::default();

        // Create multiple daemons in different states
        let initializing_daemon = DaemonRecord {
            data: test_daemon_data(),
            state: Initializing {
                started_at: chrono::Utc::now(),
            },
        };
        storage.persist_daemon(&initializing_daemon).await.unwrap();

        let running_daemon_1 = DaemonRecord {
            data: test_daemon_data(),
            state: Running {
                started_at: chrono::Utc::now(),
                last_heartbeat: chrono::Utc::now(),
                stats: DaemonStats::default(),
            },
        };
        storage.persist_daemon(&running_daemon_1).await.unwrap();

        let running_daemon_2 = DaemonRecord {
            data: test_daemon_data(),
            state: Running {
                started_at: chrono::Utc::now(),
                last_heartbeat: chrono::Utc::now(),
                stats: DaemonStats::default(),
            },
        };
        storage.persist_daemon(&running_daemon_2).await.unwrap();

        let dead_daemon = DaemonRecord {
            data: test_daemon_data(),
            state: Dead {
                started_at: chrono::Utc::now() - chrono::Duration::hours(1),
                stopped_at: chrono::Utc::now(),
                final_stats: DaemonStats::default(),
            },
        };
        storage.persist_daemon(&dead_daemon).await.unwrap();

        // Test: List all daemons
        let all = storage.list_daemons(None).await.unwrap();
        assert_eq!(all.len(), 4);

        // Test: List only running daemons
        let running = storage
            .list_daemons(Some(DaemonStatus::Running))
            .await
            .unwrap();
        assert_eq!(running.len(), 2);

        // Test: List only dead daemons
        let dead = storage
            .list_daemons(Some(DaemonStatus::Dead))
            .await
            .unwrap();
        assert_eq!(dead.len(), 1);

        // Test: List only initializing daemons
        let initializing = storage
            .list_daemons(Some(DaemonStatus::Initializing))
            .await
            .unwrap();
        assert_eq!(initializing.len(), 1);
    }
}
