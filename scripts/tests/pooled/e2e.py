#!/usr/bin/env python3
"""Exercise the real application with transaction pooling and hostile session reuse.

Regressions caught: session locks/LISTEN on query pools, schema drift, migration ledger collisions, and maintenance/worker pools
accidentally using transaction endpoints. Only the external model is mocked.
"""

import argparse
import json
import os
import signal
import socket
import subprocess
import tempfile
import threading
import time
import uuid
from concurrent.futures import ThreadPoolExecutor
from contextlib import ExitStack
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import psycopg
import requests
from psycopg import sql
from psycopg.conninfo import conninfo_to_dict
from psycopg.rows import dict_row

LEADER_LOCK = 0x4457435450524F42


def check(condition, message):
    if not condition:
        raise AssertionError(message)


def eventually(description, predicate, timeout=30):
    deadline = time.monotonic() + timeout
    while True:
        result = predicate()
        if result:
            print(f"PASS: {description}", flush=True)
            return result
        if time.monotonic() >= deadline:
            raise AssertionError(f"Timed out: {description}")
        time.sleep(0.1)


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


class Model(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        response = {
            "id": "chatcmpl-" + uuid.uuid4().hex,
            "object": "chat.completion",
            "created": int(time.time()),
            "model": body["model"],
            "choices": [
                {
                    "index": 0,
                    "message": {"role": "assistant", "content": "pool test response"},
                    "finish_reason": "stop",
                }
            ],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8},
        }
        data = json.dumps(response).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


class Application:
    def __init__(self, args, directory, config):
        self.args, self.directory, self.config = args, directory, config
        self.process = None
        self.container = "pool-test-" + uuid.uuid4().hex
        self.generation = 0

    def start(self):
        self.generation += 1
        path = self.directory / "config.json"
        path.write_text(json.dumps(self.config))
        path.chmod(0o644)
        self.log_path = self.args.artifacts / f"app-{self.generation}.log"
        env = {
            k: v
            for k, v in os.environ.items()
            if not k.startswith("DWCTL_")
            and k not in ("DATABASE_URL", "DATABASE_POOLED_URL", "DATABASE_REPLICA_URL")
        }
        env["RUST_LOG"] = "info"
        if self.args.image:
            command = [
                "docker",
                "run",
                "--rm",
                "--name",
                self.container,
                "--network",
                "host",
                "-e",
                "RUST_LOG=info",
                "-v",
                f"{path}:/app/pool-test.json:ro",
                self.args.image,
                "--config",
                "/app/pool-test.json",
            ]
        else:
            command = [str(self.args.binary.resolve()), "--config", str(path)]
        with self.log_path.open("w") as log:
            self.process = subprocess.Popen(
                command,
                cwd=self.directory,
                env=env,
                stdout=log,
                stderr=subprocess.STDOUT,
            )

        def healthy():
            check(
                self.process.poll() is None, f"application exited; see {self.log_path}"
            )
            try:
                return (
                    requests.get(
                        f"http://127.0.0.1:{self.config['port']}/healthz", timeout=1
                    ).status_code
                    == 200
                )
            except requests.ConnectionError:
                return False

        eventually("application starts, migrates, and becomes healthy", healthy, 60)

    def stop(self, strict=False):
        if not self.process:
            return
        try:
            if self.process.poll() is None:
                if self.args.image:
                    subprocess.run(
                        ["docker", "kill", "--signal=TERM", self.container],
                        check=True,
                        capture_output=True,
                        timeout=10,
                    )
                else:
                    self.process.send_signal(signal.SIGTERM)
            code = self.process.wait(timeout=30)
            if strict:
                check(code == 0, f"application shutdown returned {code}")
        finally:
            if self.process.poll() is None:
                if self.args.image:
                    subprocess.run(
                        ["docker", "rm", "-f", self.container],
                        check=False,
                        capture_output=True,
                        timeout=10,
                    )
                self.process.kill()
                self.process.wait()
            self.process = None


def characterize_pool(pooled, direct):
    """Fail if the fixture accidentally preserves session state or skips switching."""
    with (
        psycopg.connect(pooled, autocommit=True) as a,
        psycopg.connect(pooled, autocommit=True) as b,
    ):
        a.execute("BEGIN")
        first = a.execute("SELECT pg_backend_pid()").fetchone()[0]
        b.execute("BEGIN")
        second = b.execute("SELECT pg_backend_pid()").fetchone()[0]
        check(
            first != second, "two concurrent transactions must use different backends"
        )
        a.execute("COMMIT")
        b.execute("COMMIT")
        pids = {a.execute("SELECT pg_backend_pid()").fetchone()[0] for _ in range(8)}
        check(
            len(pids) == 2,
            "client must switch PostgreSQL backends between transactions",
        )
        a.execute("SET pool_test.session_marker = 'must_not_survive'")
        check(
            a.execute(
                "SELECT current_setting('pool_test.session_marker', true)"
            ).fetchone()[0]
            != "must_not_survive",
            "pooled session SET leaked",
        )
        a.execute("LISTEN pool_test_channel")
        check(
            a.execute("SELECT count(*) FROM pg_listening_channels()").fetchone()[0]
            == 0,
            "pooled LISTEN unexpectedly survived",
        )
        a.execute("SELECT pg_advisory_lock(1234567)")
        with psycopg.connect(direct, autocommit=True) as observer:
            check(
                observer.execute("SELECT pg_try_advisory_lock(1234567)").fetchone()[0],
                "pooled advisory lock unexpectedly survived",
            )
            observer.execute("SELECT pg_advisory_unlock(1234567)")
    print(
        "PASS: transaction pool switches backends and discards SET, LISTEN, and session locks",
        flush=True,
    )


def flows(app, connection, pool_admin, model_url, roles, admin_dsn):
    base = f"http://127.0.0.1:{app.config['port']}"
    session = requests.Session()

    def api(method, path, **kwargs):
        response = session.request(method, base + path, timeout=20, **kwargs)
        check(
            response.status_code in (200, 201, 204),
            f"{method} {path}: {response.status_code} {response.text[:300]}",
        )
        return response.json() if response.content else None

    def leader_held():
        # A fresh contender must NOT acquire the application's session lock.
        acquired = connection.execute(
            "SELECT pg_try_advisory_lock(%s)", (LEADER_LOCK,)
        ).fetchone()[0]
        if acquired:
            connection.execute("SELECT pg_advisory_unlock(%s)", (LEADER_LOCK,))
        return not acquired

    eventually("leader retains its advisory lock on a direct connection", leader_held)
    api(
        "POST",
        "/authentication/login",
        json={
            "email": "pool-test@example.invalid",
            "password": "local-pool-test-password",
        },
    )
    user = api("GET", "/admin/api/v1/users/current")
    api(
        "POST",
        "/admin/api/v1/transactions",
        json={
            "user_id": user["id"],
            "transaction_type": "admin_grant",
            "amount": "100",
            "source_id": "pooled-e2e-fixture-credit",
        },
    )
    endpoint = api(
        "POST",
        "/admin/api/v1/endpoints",
        json={"name": "Pool test", "url": model_url, "sync": False},
    )
    model = api(
        "POST",
        "/admin/api/v1/models",
        json={
            "type": "standard",
            "model_name": "pool-test",
            "alias": "pool-test",
            "hosted_on": endpoint["id"],
        },
    )
    group = api("POST", "/admin/api/v1/groups", json={"name": "Pool test"})
    api("POST", f"/admin/api/v1/groups/{group['id']}/users/{user['id']}")
    api("POST", f"/admin/api/v1/groups/{group['id']}/models/{model['id']}")
    key_path = f"/admin/api/v1/users/{user['id']}/api-keys"
    key = api("POST", key_path, json={"name": "Pool test", "purpose": "realtime"})
    headers = {"Authorization": "Bearer " + key["key"]}
    body = {
        "model": "pool-test",
        "messages": [{"role": "user", "content": "exercise pooled query traffic"}],
    }

    def inference():
        return requests.post(
            base + "/ai/v1/chat/completions", headers=headers, json=body, timeout=20
        )

    def routable():
        response = inference()
        if response.status_code in (401, 403, 404):
            return False
        check(
            response.status_code == 200,
            f"inference failed: {response.status_code} {response.text[:300]}",
        )
        check(
            response.json()["choices"][0]["message"]["content"] == "pool test response",
            "wrong inference response",
        )
        return True

    # Fallback sync is an hour: a broken LISTEN cannot hide behind polling.
    eventually("new model and API key propagate through LISTEN", routable, 15)
    with ThreadPoolExecutor(max_workers=16) as executor:
        responses = list(executor.map(lambda _: inference(), range(40)))
    check(all(r.status_code == 200 for r in responses), "concurrent inference failed")
    check(leader_held(), "leader lock lost after concurrent pooled traffic")
    eventually(
        "outlet persists inference logs",
        lambda: connection.execute(
            "SELECT count(*) > 0 FROM outlet.http_responses"
        ).fetchone()[0],
    )

    probe = api(
        "POST",
        "/admin/api/v1/probes",
        json={
            "name": "Pool test",
            "deployment_id": model["id"],
            "interval_seconds": 1,
            "http_method": "POST",
            "request_path": "/v1/chat/completions",
            "request_body": body,
        },
    )
    check(
        api("GET", f"/admin/api/v1/probes/{probe['id']}")["id"] == probe["id"],
        "probe read-after-write failed",
    )
    # Check actual scheduler output, not a foreground manual execute.
    eventually(
        "new probe is scheduled through LISTEN",
        lambda: connection.execute(
            "SELECT count(*) > 0 FROM probe_results WHERE probe_id = %s", (probe["id"],)
        ).fetchone()[0],
        15,
    )
    lines = "".join(
        json.dumps(
            {
                "custom_id": f"pool-{i}",
                "method": "POST",
                "url": "/v1/chat/completions",
                "body": body,
            }
        )
        + "\n"
        for i in range(3)
    )
    file = api(
        "POST",
        "/ai/v1/files",
        files={"file": ("pool.jsonl", lines, "application/jsonl")},
        data={"purpose": "batch"},
    )
    batch = api(
        "POST",
        "/ai/v1/batches",
        json={
            "input_file_id": file["id"],
            "endpoint": "/v1/chat/completions",
            "completion_window": "24h",
        },
    )

    def completed():
        result = api("GET", "/ai/v1/batches/" + batch["id"])
        check(
            result["status"] not in ("failed", "expired", "cancelled"),
            f"batch failed: {result}",
        )
        return result if result["status"] == "completed" else False

    batch = eventually(
        "underway validates and fusillade completes the batch", completed, 90
    )
    output_path = "/ai/v1/files/" + batch["output_file_id"] + "/content"
    output = session.get(base + output_path, timeout=20)
    check(output.status_code == 200, "batch output download failed")
    rows = [json.loads(line) for line in output.text.splitlines()]
    check(
        len(rows) == 3 and all(r["response"]["status_code"] == 200 for r in rows),
        "batch output incomplete",
    )
    api("DELETE", key_path + "/" + key["id"])
    eventually(
        "API-key revocation propagates through LISTEN",
        lambda: inference().status_code in (401, 403),
        15,
    )
    check(leader_held(), "leader lock no longer held before shutdown")
    pools = [
        row
        for row in pool_admin.execute("SHOW POOLS").fetchall()
        if row["user"] in roles
    ]
    check({row["user"] for row in pools} == set(roles), "missing component query pool")
    check(
        all(
            row["pool_mode"] == "transaction" and row["sv_active"] + row["sv_idle"] <= 2
            for row in pools
        ),
        "unexpected pooler mode or backend count",
    )
    print(
        "PASS: all component query pools use transaction pooling with at most two backends",
        flush=True,
    )
    app.stop(strict=True)
    check(not leader_held(), "leader lock remains held after graceful shutdown")

    # Seed an empty expired partition only in this disposable database. On the
    # next startup the real retirement daemon must detach/drop it using its
    # session-capable maintenance pool, not merely pass an endpoint attestation.
    expired_day = connection.execute("SELECT CURRENT_DATE - 2").fetchone()[0]
    connection.execute("SET search_path TO fusillade")
    connection.execute(
        "SELECT ensure_retained_response_partition(%s::date, NULL)", (expired_day,)
    )
    check(
        connection.execute(
            "SELECT state FROM retained_response_buckets WHERE delete_on=%s",
            (expired_day,),
        ).fetchone()[0]
        == "active",
        "expired partition fixture is not active",
    )
    connection.execute("SET search_path TO public")
    # Observe the real DDL session, where a transaction pool would have erased
    # SET SESSION bounds. The event trigger is confined to this disposable DB
    # and exact expired partition; application migrations are unaffected.
    partition = connection.execute(
        "SELECT 'retained_response_objects_d' || to_char(%s::date, 'YYYYMMDD')",
        (expired_day,),
    ).fetchone()[0]
    with psycopg.connect(
        admin_dsn, dbname=connection.info.dbname, autocommit=True
    ) as observer:
        observer.execute(
            "CREATE TABLE public.pool_test_maintenance_observations (command text)"
        )
        function = sql.SQL("""
            CREATE FUNCTION public.check_pool_test_maintenance() RETURNS event_trigger
            LANGUAGE plpgsql SECURITY DEFINER SET search_path TO pg_catalog AS $body$
            BEGIN
                IF session_user = {} AND strpos(current_query(), {}) > 0 THEN
                    IF current_setting('lock_timeout') = '0'
                       OR current_setting('statement_timeout') = '0' THEN
                        RAISE EXCEPTION 'retirement lost its session timeout bounds';
                    END IF;
                    INSERT INTO public.pool_test_maintenance_observations VALUES (tg_tag);
                END IF;
            END
            $body$
        """).format(sql.Literal(roles[0]), sql.Literal(partition))
        observer.execute(function)
        observer.execute("""
            CREATE EVENT TRIGGER pool_test_maintenance ON ddl_command_start
            WHEN TAG IN ('ALTER TABLE', 'DROP TABLE')
            EXECUTE FUNCTION public.check_pool_test_maintenance()
        """)
        observer.execute(
            sql.SQL(
                "GRANT SELECT ON public.pool_test_maintenance_observations TO {}"
            ).format(sql.Identifier(roles[0]))
        )
    app.start()
    eventually("leader reacquires its lock after restart", leader_held)
    eventually(
        "retention maintenance retires the expired partition",
        lambda: connection.execute(
            "SELECT state = 'retired' FROM fusillade.retained_response_buckets WHERE delete_on=%s",
            (expired_day,),
        ).fetchone()[0],
        60,
    )
    missing = connection.execute(
        "SELECT to_regclass('fusillade.' || %s) IS NULL", (partition,)
    ).fetchone()[0]
    check(missing, "retired partition was not dropped")
    commands = {
        row[0]
        for row in connection.execute(
            "SELECT command FROM public.pool_test_maintenance_observations"
        ).fetchall()
    }
    check(
        commands == {"ALTER TABLE", "DROP TABLE"},
        "retirement DDL session bounds were not observed",
    )
    check(
        api("GET", "/ai/v1/batches/" + batch["id"])["status"] == "completed",
        "completed batch lost across restart",
    )
    check(
        session.get(base + output_path, timeout=20).text == output.text,
        "batch output changed across restart",
    )
    app.stop(strict=True)
    check(not leader_held(), "leader lock remains held after final shutdown")
    print(
        "PASS: retirement DDL, persisted batch output, and graceful restart", flush=True
    )


def run(args):
    args.artifacts = args.artifacts.resolve()
    args.artifacts.mkdir(parents=True, exist_ok=True)
    admin_dsn = os.environ.get(
        "POOLED_TEST_DATABASE_URL",
        "postgres://postgres:password@127.0.0.1:5432/postgres",
    )
    admin_options = conninfo_to_dict(admin_dsn)
    check(
        admin_options.get("host", "127.0.0.1") in ("localhost", "127.0.0.1"),
        "use a local disposable PostgreSQL instance",
    )
    name = "pool_test_" + uuid.uuid4().hex[:12]
    roles = [name + suffix for suffix in ("_main", "_outlet")]
    password = uuid.uuid4().hex
    with ExitStack() as stack:
        directory = Path(
            stack.enter_context(tempfile.TemporaryDirectory(prefix="pool-e2e-"))
        )
        directory.chmod(0o755)  # The non-root application image reads its config.
        admin = stack.enter_context(psycopg.connect(admin_dsn, autocommit=True))

        def cleanup_database():
            admin.execute(
                sql.SQL("DROP DATABASE IF EXISTS {} WITH (FORCE)").format(
                    sql.Identifier(name)
                )
            )
            for role in reversed(roles):
                admin.execute(
                    sql.SQL("DROP ROLE IF EXISTS {}").format(sql.Identifier(role))
                )

        stack.callback(cleanup_database)
        for role in roles:
            admin.execute(
                sql.SQL("CREATE ROLE {} LOGIN PASSWORD {}").format(
                    sql.Identifier(role), sql.Literal(password)
                )
            )
        admin.execute(sql.SQL("GRANT {} TO {}").format(*map(sql.Identifier, roles)))
        admin.execute(
            sql.SQL("CREATE DATABASE {} OWNER {}").format(
                sql.Identifier(name), sql.Identifier(roles[0])
            )
        )
        for role, schema in zip(roles, ("public", "outlet")):
            admin.execute(
                sql.SQL("ALTER ROLE {} IN DATABASE {} SET search_path TO {}").format(
                    sql.Identifier(role), sql.Identifier(name), sql.Identifier(schema)
                )
            )
        port = free_port()
        auth = directory / "users.txt"
        auth.write_text("".join(f'"{role}" "{password}"\n' for role in roles))
        ini = directory / "pgbouncer.ini"
        ini.write_text(f"""[databases]
{name} = host=127.0.0.1 port={admin_options.get("port", "5432")} dbname={name}
[pgbouncer]
listen_addr=127.0.0.1
listen_port={port}
unix_socket_dir={directory}
auth_type=plain
auth_file={auth}
admin_users={roles[0]}
pool_mode=transaction
default_pool_size=2
max_client_conn=100
max_prepared_statements=100
server_round_robin=1
server_reset_query=DISCARD ALL
server_reset_query_always=1
ignore_startup_parameters=extra_float_digits
""")
        log = stack.enter_context(open(args.artifacts / "pgbouncer.log", "w"))
        pooler = subprocess.Popen(
            ["pgbouncer", str(ini)], stdout=log, stderr=subprocess.STDOUT
        )

        def stop_pooler():
            pooler.terminate()
            try:
                pooler.wait(timeout=10)
            except subprocess.TimeoutExpired:
                pooler.kill()
                pooler.wait()

        stack.callback(stop_pooler)

        def dsn(role, pooled=False, database=name):
            return f"postgres://{role}:{password}@127.0.0.1:{port if pooled else admin_options.get('port', '5432')}/{database}"

        def pool_ready():
            check(pooler.poll() is None, "PgBouncer exited; see pgbouncer.log")
            try:
                with psycopg.connect(dsn(roles[0], True), connect_timeout=1):
                    return True
            except psycopg.OperationalError:
                return False

        eventually("PgBouncer accepts connections", pool_ready)
        characterize_pool(dsn(roles[0], True), dsn(roles[0]))
        server = ThreadingHTTPServer(("127.0.0.1", 0), Model)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        stack.callback(server.server_close)
        stack.callback(server.shutdown)
        pool = {"max_connections": 6, "min_connections": 0, "acquire_timeout_secs": 10}
        config = {
            "host": "127.0.0.1",
            "port": free_port(),
            "secret_key": password,
            "admin_email": "pool-test@example.invalid",
            "admin_password": "local-pool-test-password",
            "model_sources": [],
            "database": {
                "type": "external",
                "url": dsn(roles[0]),
                "pooled_url": dsn(roles[0], True),
                "replica_url": dsn(roles[0], True),
                "pool": pool,
                "direct_pool": {"max_connections": 8},
                "underway_pool": {"max_connections": 20},
            },
            "enable_request_logging": True,
            "enable_analytics": True,
            "enable_otel_export": False,
            "email": {"type": "file", "path": "/tmp/pool-test-emails"},
            "auth": {
                "native": {"enabled": True, "session": {"cookie_secure": False}},
                "proxy_header": {"enabled": False},
            },
            "background_services": {
                "onwards_sync": {
                    "enabled": True,
                    "fallback_interval_milliseconds": 3600000,
                },
                "sync_workers": {"enabled": False},
                "notifications": {"webhooks": {"enabled": False}},
                "task_workers": {
                    "create_batch_workers": 2,
                    "cascade_batch_state_workers": 1,
                    "purge_user_data_workers": 1,
                },
                "batch_daemon": {
                    "enabled": "always",
                    "retained_response_retirement_enabled": True,
                    "retention": {
                        "batchless_seconds_by_service_tier": {"priority": 604800},
                        "max_late_writer_seconds": 3600,
                    },
                },
            },
        }
        config["database"]["fusillade"] = {
            "mode": "schema",
            "name": "fusillade",
            "pool": pool,
        }
        for role, schema in zip(roles[1:], ("outlet",)):
            config["database"][schema] = {
                "mode": "schema",
                "name": schema,
                "pooled_url": dsn(role, True),
                "replica_url": dsn(role, True),
                "pool": pool,
            }
        app = Application(args, directory, config)
        stack.callback(app.stop)
        app.start()
        direct = stack.enter_context(psycopg.connect(dsn(roles[0]), autocommit=True))
        pool_admin = stack.enter_context(
            psycopg.connect(
                dsn(roles[0], True, "pgbouncer"), autocommit=True, row_factory=dict_row
            )
        )
        flows(
            app,
            direct,
            pool_admin,
            f"http://127.0.0.1:{server.server_port}",
            roles,
            admin_dsn,
        )
        with psycopg.connect(dsn(roles[0], True), autocommit=True) as shared:
            for _ in range(4):
                schema, can_manage_roles = shared.execute(
                    "SELECT current_schema(), rolcreaterole FROM pg_roles WHERE rolname = current_user"
                ).fetchone()
                check(
                    schema == "public",
                    "Fusillade changed the shared role's default schema",
                )
                check(
                    not can_manage_roles,
                    "application must not require role-management privileges",
                )
        print(
            "PASS: shared application role retains public schema without CREATEROLE",
            flush=True,
        )
    print("PASS: all pooled application E2E checks", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    target = parser.add_mutually_exclusive_group(required=True)
    target.add_argument("--binary", type=Path, help="locally built dwctl executable")
    target.add_argument(
        "--image", help="built dwctl image (Linux Docker host networking)"
    )
    parser.add_argument("--artifacts", type=Path, default=Path("pooled-e2e-results"))
    run(parser.parse_args())
