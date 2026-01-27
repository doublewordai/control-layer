#!/bin/bash
# Recover missing credit transactions and http_analytics records from batches that don't have http_analytics entries.
# This is needed due to outlet being disabled from Jan 23 to Jan 25, 2026, causing missing analytics and billing records.
# Usage: ./recover_billing_from_missing_http_analytics.sh <database_url>. It only inserts one transaction and one http analytics
# record per batch to avoid overwhelming the database.

set -e

DB_URL="${1:-}"
MODEL="${2:-Qwen/Qwen3-VL-235B-A22B-Instruct-FP8}"

if [ -z "$DB_URL" ]; then
    echo "Usage: $0 <database_url>"
    exit 1
fi

echo "Simple Batch Backfill - One transaction per batch"
echo ""

PROCESSED=0

while true; do
    # Step 1: Find next unprocessed batch and calculate totals
    BATCH_INFO=$(psql "$DB_URL" -t -A -c "
        SELECT
            b.id,
            b.created_by::uuid,
            MIN(fr.completed_at),
            COUNT(*),
            SUM((fr.response_body::jsonb->'usage'->>'prompt_tokens')::bigint),
            SUM((fr.response_body::jsonb->'usage'->>'completion_tokens')::bigint),
            (SUM((fr.response_body::jsonb->'usage'->>'prompt_tokens')::bigint) * 0.00000010 +
             SUM((fr.response_body::jsonb->'usage'->>'completion_tokens')::bigint) * 0.00000040)
        FROM fusillade.batches b
        JOIN fusillade.requests fr ON fr.batch_id = b.id
        WHERE b.created_at >= '2026-01-23 00:00:00+00'
          AND b.created_at < '2026-01-26 00:00:00+00'
          AND fr.model = '$MODEL'
          AND fr.response_status = 200
          AND NOT EXISTS (SELECT 1 FROM http_analytics ha WHERE ha.fusillade_batch_id = b.id LIMIT 1)
        GROUP BY b.id, b.created_by
        LIMIT 1;
    " 2>&1)

    # Check if we found a batch
    if [ -z "$BATCH_INFO" ]; then
        echo "✓ All batches processed!"
        break
    fi

    # Parse batch info
    BATCH_ID=$(echo "$BATCH_INFO" | cut -d'|' -f1)
    USER_ID=$(echo "$BATCH_INFO" | cut -d'|' -f2)
    TIMESTAMP=$(echo "$BATCH_INFO" | cut -d'|' -f3)
    NUM_REQUESTS=$(echo "$BATCH_INFO" | cut -d'|' -f4)
    PROMPT_TOKENS=$(echo "$BATCH_INFO" | cut -d'|' -f5)
    COMPLETION_TOKENS=$(echo "$BATCH_INFO" | cut -d'|' -f6)
    COST=$(echo "$BATCH_INFO" | cut -d'|' -f7)

    # Step 2: Create analytics record
    psql "$DB_URL" -c "
        INSERT INTO http_analytics (
            instance_id, correlation_id, timestamp, method, uri, model, status_code,
            prompt_tokens, completion_tokens, user_id, input_price_per_token,
            output_price_per_token, fusillade_batch_id, request_origin, batch_sla,
            response_type, created_at
        ) VALUES (
            '00000000-0000-0000-0000-000000000000'::uuid,
            ('x' || substr(md5('$BATCH_ID'), 1, 15))::bit(60)::bigint,
            '$TIMESTAMP',
            'POST',
            '/ai/v1/chat/completions',
            '$MODEL',
            200,
            $PROMPT_TOKENS,
            $COMPLETION_TOKENS,
            '$USER_ID'::uuid,
            0.0,
            0.0,
            '$BATCH_ID'::uuid,
            'batch',
            '24h',
            'success',
            '$TIMESTAMP'
        );
    " >/dev/null 2>&1

    # Step 3: Create credit transaction
    psql "$DB_URL" -c "
        INSERT INTO credits_transactions (
            user_id, transaction_type, amount, source_id, balance_after,
            previous_transaction_id, description, created_at, fusillade_batch_id
        )
        SELECT
            '$USER_ID'::uuid,
            'usage',
            $COST,
            'batch:$BATCH_ID',
            NULL,
            (SELECT id FROM credits_transactions WHERE user_id = '$USER_ID'::uuid ORDER BY created_at DESC, seq DESC LIMIT 1),
            'Backfilled batch ($NUM_REQUESTS requests) - outlet outage Jan 23-25, 2026',
            '$TIMESTAMP',
            '$BATCH_ID'::uuid;
    " >/dev/null 2>&1

    ((PROCESSED++))
    echo "✓ Batch $PROCESSED: ${BATCH_ID:0:8}... ($NUM_REQUESTS requests, \$$COST)"

    if [ $((PROCESSED % 10)) -eq 0 ]; then
        echo ""
        echo "Progress: $PROCESSED batches"
        echo ""
    fi
done

echo ""
echo "Done! Processed $PROCESSED batches"
