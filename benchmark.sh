#!/usr/bin/env bash
set -euo pipefail

PGBIN="/usr/lib/postgresql/16/bin"
PGUSER="postgres"

# OPTIMIZATION: Use tmpfs (RAM) for postgres data
PGDATA="/dev/shm/pg_test_data"
PGSOCK="/dev/shm/pg_test_sock"

echo "=== Stopping and removing old postgres..."
runuser -u $PGUSER -- $PGBIN/pg_ctl -D "$PGDATA" stop -m immediate 2>/dev/null || true
rm -rf "$PGDATA" "$PGSOCK"
mkdir -p "$PGDATA" "$PGSOCK"
chown $PGUSER:$PGUSER "$PGDATA" "$PGSOCK"

echo "=== Initializing fresh postgres on tmpfs..."
runuser -u $PGUSER -- $PGBIN/initdb -D "$PGDATA" --no-locale --encoding=UTF8 > /dev/null

echo "=== Starting postgres with all optimizations..."
runuser -u $PGUSER -- $PGBIN/postgres -D "$PGDATA" \
  -c fsync=off \
  -c full_page_writes=off \
  -c synchronous_commit=off \
  -c wal_level=minimal \
  -c max_wal_senders=0 \
  -c checkpoint_timeout=1d \
  -c shared_buffers=256MB \
  -c listen_addresses='localhost' \
  -c port=5432 \
  -c unix_socket_directories="$PGSOCK" \
  > /dev/null 2>&1 &
PGPID=$!
sleep 2

# Wait for postgres to be ready
echo "=== Waiting for postgres..."
until $PGBIN/pg_isready -h localhost -p 5432 > /dev/null 2>&1; do
  sleep 0.5
done
echo "Postgres ready!"

# Set up .env files for sqlx (test harness will use this)
echo "DATABASE_URL=postgres://$PGUSER@localhost:5432/test" > dwctl/.env
echo "DATABASE_URL=postgres://$PGUSER@localhost:5432/test" > fusillade/.env

# Set env vars for tests
export DATABASE_URL="postgres://$PGUSER@localhost:5432/test"
export TEST_DATABASE_URL="postgres://$PGUSER@localhost:5432/test"

echo "=== Compiling tests..."
cargo test --lib --no-run 2>&1 | grep -E "(Compiling|Finished)" || true

echo ""
echo "=== RUNNING TEST SUITE ==="
start=$(date +%s.%N)
cargo test --lib 2>&1 | grep -E "(test result|running)" || true
end=$(date +%s.%N)

runtime=$(echo "$end - $start" | bc)
echo ""
echo "=== Test suite completed in ${runtime}s ==="

echo "=== Cleaning up..."
runuser -u $PGUSER -- $PGBIN/pg_ctl -D "$PGDATA" stop -m immediate > /dev/null 2>&1 || true
rm -rf "$PGDATA" "$PGSOCK"
