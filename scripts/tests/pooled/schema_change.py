"""Schema changes must preserve results on retained prepared server statements."""

import uuid

import psycopg
import requests


def characterize_stale_plans(direct, pooled_dsn):
    """Negative control: prove both existing and fresh clients see the old bug."""
    direct.execute("CREATE TABLE pool_test_plan_shape (id integer)")
    query = "SELECT * FROM pool_test_plan_shape"
    with (
        psycopg.connect(pooled_dsn, autocommit=True) as old,
        psycopg.connect(pooled_dsn, autocommit=True) as other,
    ):
        # Hold both backends so the test cannot pass by using an unprimed one.
        old.execute("BEGIN")
        other.execute("BEGIN")
        pids = set()
        for client in (old, other):
            pids.add(client.execute("SELECT pg_backend_pid()").fetchone()[0])
            client.execute(query, prepare=True).fetchall()
        assert len(pids) == 2
        old.execute("COMMIT")
        other.execute("COMMIT")
        direct.execute("ALTER TABLE pool_test_plan_shape ADD COLUMN added text")
        with psycopg.connect(pooled_dsn, autocommit=True) as fresh:
            for client in (old, fresh):
                assert client.execute("SELECT pg_backend_pid()").fetchone()[0] in pids
                try:
                    client.execute(query, prepare=True).fetchall()
                except psycopg.errors.FeatureNotSupported as error:
                    assert "cached plan must not change result type" in str(error)
                else:
                    raise AssertionError("stale wildcard plan unexpectedly succeeded")
    # All original clients are gone; only the pooler's backend sessions remain.
    with psycopg.connect(pooled_dsn, autocommit=True) as replacement:
        assert replacement.execute("SELECT pg_backend_pid()").fetchone()[0] in pids
        try:
            replacement.execute(query, prepare=True).fetchall()
        except psycopg.errors.FeatureNotSupported as error:
            assert "cached plan must not change result type" in str(error)
        else:
            raise AssertionError(
                "closing all application clients unexpectedly cleared backend plans"
            )
    direct.execute("DROP TABLE pool_test_plan_shape")
    print(
        "PASS: wildcard plan fails after ADD COLUMN for existing and fresh clients",
        flush=True,
    )


def verify_models_schema_change(app, direct):
    """Exercise SQLx and the HTTP handler before/after DDL and a pod replacement."""
    base = f"http://127.0.0.1:{app.config['port']}"
    app.start()
    session = requests.Session()
    response = session.post(
        base + "/authentication/login",
        json={
            "email": "pool-test@example.invalid",
            "password": "local-pool-test-password",
        },
        timeout=20,
    )
    response.raise_for_status()

    def listings():
        results = []
        # Exercise both populated and empty pages and cycle both server connections.
        for _ in range(8):
            for skip in (0, 100):
                response = session.get(
                    base + f"/admin/api/v1/models?limit=100&skip={skip}", timeout=20
                )
                assert (
                    response.status_code == 200
                ), f"models list failed: {response.status_code} {response.text[:300]}"
                results.append(response.json())
        return results

    def writes():
        endpoint = str(
            direct.execute("SELECT id FROM inference_endpoints LIMIT 1").fetchone()[0]
        )
        for _ in range(4):
            alias = "schema-test-" + uuid.uuid4().hex
            response = session.post(
                base + "/admin/api/v1/models",
                json={
                    "type": "standard",
                    "model_name": alias,
                    "alias": alias,
                    "hosted_on": endpoint,
                },
                timeout=20,
            )
            assert (
                response.status_code == 200
            ), f"model create failed: {response.status_code} {response.text[:300]}"
            model = response.json()
            response = session.patch(
                base + "/admin/api/v1/models/" + model["id"],
                json={"description": "schema compatibility"},
                timeout=20,
            )
            assert (
                response.status_code == 200
            ), f"model update failed: {response.status_code} {response.text[:300]}"
            assert response.json()["description"] == "schema compatibility"

    writes()
    before = listings()
    direct.execute(
        "ALTER TABLE deployed_models ADD COLUMN pool_test_future_column text"
    )
    try:
        assert listings() == before, "additive DDL changed the models API response"
        writes()
        before_restart = listings()
        app.stop(strict=True)
        # Keep PgBouncer and its PostgreSQL connections running across the restart.
        app.start()
        assert (
            listings() == before_restart
        ), "new application client failed after additive DDL"
        writes()
    finally:
        app.stop()
        direct.execute(
            "ALTER TABLE deployed_models DROP COLUMN pool_test_future_column"
        )
        session.close()
    print(
        "PASS: models API survives additive DDL and application replacement with retained plans",
        flush=True,
    )


def characterize_returning_and_type_changes(direct, pooled_dsn):
    """Protect the fixture against write-result and incompatible-type blind spots."""
    direct.execute("CREATE TABLE pool_test_write_shape (id integer)")
    direct.execute("INSERT INTO pool_test_write_shape VALUES (1)")
    wildcard = "UPDATE pool_test_write_shape SET id = id RETURNING *"
    stable = "UPDATE pool_test_write_shape SET id = id RETURNING id"
    read = "SELECT id FROM pool_test_write_shape"
    with psycopg.connect(pooled_dsn, autocommit=True) as client:
        # Round-robin visits both backends; execute each query on both.
        for query in (wildcard, stable, read):
            for _ in range(4):
                assert client.execute(query, prepare=True).fetchall() == [(1,)]
        direct.execute("ALTER TABLE pool_test_write_shape ADD COLUMN added text")
        for _ in range(4):
            try:
                client.execute(wildcard, prepare=True).fetchall()
            except psycopg.errors.FeatureNotSupported as error:
                assert "cached plan must not change result type" in str(error)
            else:
                raise AssertionError(
                    "RETURNING wildcard unexpectedly survived ADD COLUMN"
                )
        with psycopg.connect(pooled_dsn, autocommit=True) as fresh:
            for connection in (client, fresh):
                for query in (stable, read):
                    for _ in range(4):
                        assert connection.execute(query, prepare=True).fetchall() == [
                            (1,)
                        ]
        direct.execute("ALTER TABLE pool_test_write_shape ALTER COLUMN id TYPE bigint")
        # Explicit projection protects additive DDL, not changing selected types.
        try:
            client.execute(read, prepare=True).fetchall()
        except psycopg.errors.FeatureNotSupported as error:
            assert "cached plan must not change result type" in str(error)
        else:
            raise AssertionError("selected column type change unexpectedly succeeded")
    direct.execute("DROP TABLE pool_test_write_shape")
    print(
        "PASS: RETURNING wildcard and incompatible type failures reproduced; explicit results survive ADD COLUMN",
        flush=True,
    )
