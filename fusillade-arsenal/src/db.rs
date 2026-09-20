use std::fmt;
use std::future::Future;
use std::sync::Arc;

use either::Either;
use futures::TryStreamExt;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use sqlx::postgres::{PgListener, PgPool};
use sqlx::{
    Database, Describe, Error as SqlxError, Execute, Executor, Postgres, Transaction, pool,
};

use crate::DbRetryConfig;

#[derive(Clone)]
pub(crate) struct RetryingPgPool {
    pool: PgPool,
    retry_config: DbRetryConfig,
    schema: Option<Arc<str>>,
}

impl RetryingPgPool {
    pub(crate) fn new(pool: &PgPool, retry_config: &DbRetryConfig) -> Self {
        Self {
            pool: pool.clone(),
            retry_config: retry_config.clone(),
            schema: None,
        }
    }

    pub(crate) fn with_schema(mut self, schema: Option<Arc<str>>) -> Self {
        self.schema = schema;
        self
    }

    async fn acquire(&self) -> Result<QueryConnection, SqlxError> {
        match self.schema.as_deref() {
            Some(schema) => Ok(QueryConnection::Transaction(
                begin_transaction_in_schema(&self.pool, &self.retry_config, Some(schema)).await?,
            )),
            None => Ok(QueryConnection::Connection(
                acquire_connection(&self.pool, &self.retry_config).await?,
            )),
        }
    }
}

// The transaction owns the backend until the query finishes. Dropping a stream
// or cancelling an operation rolls it back, including its local schema setting.
enum QueryConnection {
    Connection(pool::PoolConnection<Postgres>),
    Transaction(Transaction<'static, Postgres>),
}

impl QueryConnection {
    fn connection(&mut self) -> &mut sqlx::PgConnection {
        match self {
            Self::Connection(connection) => connection,
            Self::Transaction(transaction) => transaction,
        }
    }

    async fn finish(self) -> Result<(), SqlxError> {
        match self {
            Self::Connection(_) => Ok(()),
            Self::Transaction(transaction) => transaction.commit().await,
        }
    }
}

impl fmt::Debug for RetryingPgPool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("RetryingPgPool").finish()
    }
}

impl<'p> Executor<'p> for RetryingPgPool {
    type Database = Postgres;

    fn fetch_many<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> BoxStream<
        'e,
        Result<
            Either<<Self::Database as Database>::QueryResult, <Self::Database as Database>::Row>,
            SqlxError,
        >,
    >
    where
        E: 'q + Execute<'q, Self::Database>,
    {
        Box::pin(async_stream::try_stream! {
            let mut connection = self.acquire().await?;
            let mut stream = connection.connection().fetch_many(query);

            while let Some(item) = stream.try_next().await? {
                yield item;
            }
            drop(stream);
            connection.finish().await?;
        })
    }

    fn fetch_optional<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> BoxFuture<'e, Result<Option<<Self::Database as Database>::Row>, SqlxError>>
    where
        E: 'q + Execute<'q, Self::Database>,
    {
        Box::pin(async move {
            let mut connection = self.acquire().await?;
            let result = connection.connection().fetch_optional(query).await?;
            connection.finish().await?;
            Ok(result)
        })
    }

    fn prepare_with<'e, 'q: 'e>(
        self,
        sql: &'q str,
        parameters: &'e [<Self::Database as Database>::TypeInfo],
    ) -> BoxFuture<'e, Result<<Self::Database as Database>::Statement<'q>, SqlxError>> {
        Box::pin(async move {
            let mut connection = self.acquire().await?;
            let result = connection
                .connection()
                .prepare_with(sql, parameters)
                .await?;
            connection.finish().await?;
            Ok(result)
        })
    }

    #[doc(hidden)]
    fn describe<'e, 'q: 'e>(
        self,
        sql: &'q str,
    ) -> BoxFuture<'e, Result<Describe<Self::Database>, SqlxError>> {
        Box::pin(async move {
            let mut connection = self.acquire().await?;
            let result = connection.connection().describe(sql).await?;
            connection.finish().await?;
            Ok(result)
        })
    }
}

pub(crate) async fn acquire_connection(
    pool: &PgPool,
    retry_config: &DbRetryConfig,
) -> Result<pool::PoolConnection<Postgres>, SqlxError> {
    retry_sqlx_pool_acquire(retry_config, || pool.acquire()).await
}

pub(crate) async fn begin_transaction(
    pool: &PgPool,
    retry_config: &DbRetryConfig,
) -> Result<Transaction<'static, Postgres>, SqlxError> {
    retry_sqlx_pool_acquire(retry_config, || pool.begin()).await
}

/// Select the component schema only for this transaction. Quote the identifier
/// because SET does not accept parameters. Unlike SELECT set_config, SET LOCAL
/// does not acquire a snapshot before callers choose their isolation level.
pub(crate) async fn begin_transaction_in_schema(
    pool: &PgPool,
    retry_config: &DbRetryConfig,
    schema: Option<&str>,
) -> Result<Transaction<'static, Postgres>, SqlxError> {
    let mut transaction = begin_transaction(pool, retry_config).await?;
    if let Some(schema) = schema {
        sqlx::query(&format!(
            "SET LOCAL search_path TO \"{}\"",
            schema.replace('"', "\"\"")
        ))
        .execute(&mut *transaction)
        .await?;
    }
    Ok(transaction)
}

pub(crate) async fn connect_listener(
    pool: &PgPool,
    retry_config: &DbRetryConfig,
) -> Result<PgListener, SqlxError> {
    retry_sqlx_pool_acquire(retry_config, || PgListener::connect_with(pool)).await
}

async fn retry_sqlx_pool_acquire<T, Op, Fut>(
    config: &DbRetryConfig,
    mut operation: Op,
) -> Result<T, SqlxError>
where
    Op: FnMut() -> Fut,
    Fut: Future<Output = Result<T, SqlxError>>,
{
    for delay in config.retry_delays() {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) if is_retryable_sqlx_error(&error) => {
                if !delay.is_zero() {
                    tokio::time::sleep(*delay).await;
                }
            }
            Err(error) => return Err(error),
        }
    }

    operation().await
}

fn is_retryable_sqlx_error(error: &SqlxError) -> bool {
    crate::is_retryable_db_error_message(&error.to_string())
}

#[cfg(test)]
mod schema_tests {
    use super::*;
    use std::sync::Arc;

    #[sqlx::test]
    async fn schema_executor_commits_writes_without_leaking_search_path(pool: PgPool) {
        pool.execute("CREATE SCHEMA component; CREATE TABLE public.pool_schema_test(value int); CREATE TABLE component.pool_schema_test(value int)")
            .await.unwrap();
        let shared = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with(pool.connect_options().as_ref().clone())
            .await
            .unwrap();
        let executor = RetryingPgPool::new(&shared, &DbRetryConfig::default())
            .with_schema(Some(Arc::from("component")));
        sqlx::query("INSERT INTO pool_schema_test VALUES (42)")
            .execute(executor.clone())
            .await
            .unwrap();
        let value: i32 = sqlx::query_scalar("SELECT value FROM pool_schema_test")
            .fetch_one(executor)
            .await
            .unwrap();
        assert_eq!(value, 42);
        let schema: String = sqlx::query_scalar("SELECT current_schema()")
            .fetch_one(&shared)
            .await
            .unwrap();
        assert_eq!(schema, "public");
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM public.pool_schema_test")
            .fetch_one(&shared)
            .await
            .unwrap();
        assert_eq!(count, 0);
        shared.close().await;
    }
    #[sqlx::test]
    async fn failed_query_and_dropped_stream_restore_shared_schema(pool: PgPool) {
        pool.execute("CREATE SCHEMA component; CREATE TABLE component.pool_schema_test(value int)")
            .await
            .unwrap();
        let shared = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with(pool.connect_options().as_ref().clone())
            .await
            .unwrap();
        let executor = RetryingPgPool::new(&shared, &DbRetryConfig::default())
            .with_schema(Some(Arc::from("component")));
        assert!(
            sqlx::query("SELECT 1 / 0")
                .execute(executor.clone())
                .await
                .is_err()
        );
        let schema: String = sqlx::query_scalar("SELECT current_schema()")
            .fetch_one(&shared)
            .await
            .unwrap();
        assert_eq!(schema, "public");
        let mut stream = sqlx::query(
            "INSERT INTO pool_schema_test SELECT generate_series(1, 100) RETURNING value",
        )
        .fetch(executor);
        assert!(stream.try_next().await.unwrap().is_some());
        drop(stream);
        let (schema, count): (String, i64) = sqlx::query_as(
            "SELECT current_schema(), (SELECT count(*) FROM component.pool_schema_test)",
        )
        .fetch_one(&shared)
        .await
        .unwrap();
        assert_eq!(schema, "public");
        assert_eq!(
            count, 0,
            "dropping an unfinished stream rolls back its transaction"
        );
        shared.close().await;
    }
    #[sqlx::test]
    async fn schema_selection_allows_snapshot_isolation_and_quoted_names(pool: PgPool) {
        pool.execute(r#"CREATE SCHEMA "component""quoted""#)
            .await
            .unwrap();
        let mut transaction = begin_transaction_in_schema(
            &pool,
            &DbRetryConfig::default(),
            Some("component\"quoted"),
        )
        .await
        .unwrap();
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *transaction)
            .await
            .unwrap();
        let schema: String = sqlx::query_scalar("SELECT current_schema()")
            .fetch_one(&mut *transaction)
            .await
            .unwrap();
        assert_eq!(schema, "component\"quoted");
        transaction.rollback().await.unwrap();
    }
}
