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
echo "=== TIMING: Compilation phase ==="
compile_start=$(date +%s.%N)
cargo test --lib --no-run
compile_end=$(date +%s.%N)
compile_time=$(echo "$compile_end - $compile_start" | bc)
echo "Compilation took: ${compile_time}s"

echo ""
echo "=== TIMING: Test execution phase ==="
exec_start=$(date +%s.%N)
cargo test --lib -- --nocapture 2>&1 | grep -E "test result|running [0-9]+" || true
exec_end=$(date +%s.%N)
exec_time=$(echo "$exec_end - $exec_start" | bc)
echo "Test execution took: ${exec_time}s"

echo ""
echo "=== SUMMARY ==="
echo "Compilation:     ${compile_time}s"
echo "Test execution:  ${exec_time}s"
total_time=$(echo "$compile_time + $exec_time" | bc)
echo "Total:           ${total_time}s"

compile_pct=$(echo "scale=1; 100 * $compile_time / $total_time" | bc)
exec_pct=$(echo "scale=1; 100 * $exec_time / $total_time" | bc)
echo ""
echo "Compilation is ${compile_pct}% of total time"
echo "Execution is ${exec_pct}% of total time"

runuser -u $PGUSER -- $PGBIN/pg_ctl -D "$PGDATA" stop -m immediate > /dev/null 2>&1 || true
rm -rf "$PGDATA" "$PGSOCK"
