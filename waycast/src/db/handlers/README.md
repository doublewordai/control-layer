Handling connections here is a bit tricky
If you're doing atomic operations, you want to use a transaction to make sure they all commit or none do.
If you're just calling one repo to do one thing you can just use a pool.
The type coercion is a bit finicky to support both of these, examples are below.


```
// pooled connection
let mut pooled = pool.acquire().await?;
let mut repo = ApiKeys::new(&mut *pooled);
let ids = repo.get_api_key_deployments(api_key_id).await?;

// inside a transaction
let mut tx = pool.begin().await?;
let conn = tx.acquire().await?; // &mut PgConnection
let mut repo_tx = ApiKeys::new(conn);
let ids2 = repo_tx.get_api_key_deployments(api_key_id).await?;

```


Note that these handlers are designed so that every individual function is atomic - this mostly applies to the update methods which make use of coalesce, but users.update requires the use of a (sub)transaction.