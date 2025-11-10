#!/usr/bin/env bash
set -euo pipefail

PGBIN="/usr/lib/postgresql/16/bin"
PGUSER="postgres"
PGDATA="/dev/shm/pg_test_data"
PGSOCK="/dev/shm/pg_test_sock"

echo "=== Setting up postgres..."
runuser -u $PGUSER -- $PGBIN/pg_ctl -D "$PGDATA" stop -m immediate 2>/dev/null || true
rm -rf "$PGDATA" "$PGSOCK"
mkdir -p "$PGDATA" "$PGSOCK"
chown $PGUSER:$PGUSER "$PGDATA" "$PGSOCK"

runuser -u $PGUSER -- $PGBIN/initdb -D "$PGDATA" --no-locale --encoding=UTF8 > /dev/null

runuser -u $PGUSER -- $PGBIN/postgres -D "$PGDATA" \
  -c fsync=off \
  -c listen_addresses='localhost' \
  -c port=5432 \
  -c unix_socket_directories="$PGSOCK" \
  > /dev/null 2>&1 &
PGPID=$!

until $PGBIN/pg_isready -h localhost -p 5432 > /dev/null 2>&1; do
  sleep 0.5
done

echo "DATABASE_URL=postgres://$PGUSER@localhost:5432/test" > dwctl/.env
echo "DATABASE_URL=postgres://$PGUSER@localhost:5432/test" > fusillade/.env
export DATABASE_URL="postgres://$PGUSER@localhost:5432/test"
export TEST_DATABASE_URL="postgres://$PGUSER@localhost:5432/test"

echo ""
echo "=== Test Run #1: Fresh compilation (no cache) ==="
cargo clean
/usr/bin/time -v cargo test --lib 2>&1 | grep -E "(Elapsed|Maximum resident|test result)" | head -10

echo ""
echo "=== Test Run #2: Incremental (with cache) ==="
touch dwctl/src/lib.rs  # Force minimal recompile
/usr/bin/time -v cargo test --lib 2>&1 | grep -E "(Elapsed|Maximum resident|test result)" | head -10

echo ""
echo "=== Test Run #3: No changes (cached) ==="
/usr/bin/time -v cargo test --lib 2>&1 | grep -E "(Elapsed|Maximum resident|test result)" | head -10

runuser -u $PGUSER -- $PGBIN/pg_ctl -D "$PGDATA" stop -m immediate > /dev/null 2>&1 || true
rm -rf "$PGDATA" "$PGSOCK"
