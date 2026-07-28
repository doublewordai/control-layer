use std::fmt;
use std::future::Future;

use either::Either;
use futures::TryStreamExt;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use sqlx::postgres::{PgListener, PgPool};
use sqlx::{
    Database, Describe, Error as SqlxError, Execute, Executor, Postgres, Transaction, pool,
};

use crate::DbRetryConfig;
use crate::postgres::{ResponseAdmissionPermit, ResponseAdmissionRequest};

#[derive(Clone)]
pub(crate) struct RetryingPgPool {
    pool: PgPool,
    retry_config: DbRetryConfig,
    admission: Option<ResponseAdmissionRequest>,
}

impl RetryingPgPool {
    pub(crate) fn new(pool: &PgPool, retry_config: &DbRetryConfig) -> Self {
        Self {
            pool: pool.clone(),
            retry_config: retry_config.clone(),
            admission: None,
        }
    }

    pub(crate) fn with_admission(mut self, admission: ResponseAdmissionRequest) -> Self {
        self.admission = Some(admission);
        self
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
            let (mut connection, _admission_permit) =
                acquire_connection_with_admission(
                    &self.pool,
                    &self.retry_config,
                    self.admission.as_ref(),
                )
                .await?;
            let mut stream = connection.fetch_many(query);

            while let Some(item) = stream.try_next().await? {
                yield item;
            }
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
            let (mut connection, _admission_permit) = acquire_connection_with_admission(
                &self.pool,
                &self.retry_config,
                self.admission.as_ref(),
            )
            .await?;
            connection.fetch_optional(query).await
        })
    }

    fn prepare_with<'e, 'q: 'e>(
        self,
        sql: &'q str,
        parameters: &'e [<Self::Database as Database>::TypeInfo],
    ) -> BoxFuture<'e, Result<<Self::Database as Database>::Statement<'q>, SqlxError>> {
        Box::pin(async move {
            let (mut connection, _admission_permit) = acquire_connection_with_admission(
                &self.pool,
                &self.retry_config,
                self.admission.as_ref(),
            )
            .await?;
            connection.prepare_with(sql, parameters).await
        })
    }

    #[doc(hidden)]
    fn describe<'e, 'q: 'e>(
        self,
        sql: &'q str,
    ) -> BoxFuture<'e, Result<Describe<Self::Database>, SqlxError>> {
        Box::pin(async move {
            let (mut connection, _admission_permit) = acquire_connection_with_admission(
                &self.pool,
                &self.retry_config,
                self.admission.as_ref(),
            )
            .await?;
            connection.describe(sql).await
        })
    }
}

async fn acquire_connection_with_admission(
    pool: &PgPool,
    retry_config: &DbRetryConfig,
    admission: Option<&ResponseAdmissionRequest>,
) -> Result<
    (
        pool::PoolConnection<Postgres>,
        Option<ResponseAdmissionPermit>,
    ),
    SqlxError,
> {
    retry_sqlx_pool_acquire(retry_config, || async {
        let permit = match admission {
            Some(request) => Some(
                request
                    .acquire()
                    .await
                    .map_err(|error| SqlxError::Protocol(error.to_string()))?,
            ),
            None => None,
        };

        // `permit` is local to this attempt. A pool-acquire failure drops it
        // before `retry_sqlx_pool_acquire` sleeps.
        let connection = pool.acquire().await?;
        Ok((connection, permit))
    })
    .await
}

pub(crate) async fn begin_transaction(
    pool: &PgPool,
    retry_config: &DbRetryConfig,
) -> Result<Transaction<'static, Postgres>, SqlxError> {
    retry_sqlx_pool_acquire(retry_config, || pool.begin()).await
}

pub(crate) async fn begin_admitted_transaction(
    pool: &PgPool,
    retry_config: &DbRetryConfig,
    admission: &ResponseAdmissionRequest,
) -> Result<(Transaction<'static, Postgres>, ResponseAdmissionPermit), SqlxError> {
    retry_sqlx_pool_acquire(retry_config, || async {
        let permit = admission
            .acquire()
            .await
            .map_err(|error| SqlxError::Protocol(error.to_string()))?;

        // As above, the attempt owns the permit so an error releases it before
        // the retry cadence sleeps.
        let transaction = pool.begin().await?;
        Ok((transaction, permit))
    })
    .await
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
