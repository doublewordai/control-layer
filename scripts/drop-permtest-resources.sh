#!/bin/bash

# Cleanup script for permission test resources (models, endpoints, files)
# Deletes any resources with -permtest suffix or from permtest models

# Wait for server to be ready
echo "Waiting for server to be ready..." >&2
for i in {1..30}; do
  if curl -s -o /dev/null -w "%{http_code}" http://localhost:3001/health | grep -q "200"; then
    echo "✅ Server is ready" >&2
    break
  fi
  if [ $i -eq 30 ]; then
    echo "⚠️  Server not ready after 30 seconds, cleanup may fail" >&2
  fi
  sleep 1
done

# Extract admin credentials from config.yaml
ADMIN_EMAIL=$(grep 'admin_email:' config.yaml | sed 's/.*admin_email:[ ]*"\(.*\)"/\1/')
ADMIN_PASSWORD=$(grep 'admin_password:' config.yaml | sed 's/.*admin_password:[ ]*"\(.*\)"/\1/')

if [ -z "$ADMIN_EMAIL" ] || [ -z "$ADMIN_PASSWORD" ]; then
  echo "Failed to extract admin credentials from config.yaml" >&2
  exit 1
fi

# Generate admin JWT for authentication
echo "Generating admin JWT..." >&2
ADMIN_JWT=$(EMAIL=$ADMIN_EMAIL PASSWORD=$ADMIN_PASSWORD ./scripts/login.sh 2>/dev/null)

if [ -z "$ADMIN_JWT" ]; then
  echo "⚠️  Failed to generate admin JWT - server may not be ready" >&2
  exit 0  # Exit gracefully, don't fail the test run
fi

echo "Cleaning up permission test resources..." >&2

# Delete test files first (files from permtest JSONL files)
echo "Fetching files..." >&2
FILES_RESPONSE=$(curl -s -X GET "http://localhost:3001/ai/v1/files?limit=100" \
  -b "dwctl_session=${ADMIN_JWT}")

DELETED_FILES=0
# Check if response has .data field (paginated response)
if echo "$FILES_RESPONSE" | jq -e '.data' >/dev/null 2>&1; then
  # Files that match our test JSONL filenames
  FILES=$(echo "$FILES_RESPONSE" | jq -r '.data[]? | select(.filename | test("^(gpt4-batch|restricted-batch|mixed-batch)\\.jsonl$")) | .id')
  if [ -n "$FILES" ]; then
    while read -r file_id; do
      echo "  Deleting file: $file_id" >&2
      if curl -s -o /dev/null -w "%{http_code}" -X DELETE "http://localhost:3001/ai/v1/files/${file_id}" \
        -b "dwctl_session=${ADMIN_JWT}" | grep -q "200"; then
        ((DELETED_FILES++))
      else
        echo "    ⚠️  Failed to delete file $file_id" >&2
      fi
    done <<< "$FILES"
  fi
else
  echo "  ⚠️  Unexpected files API response format" >&2
fi

# Delete test models (deployments) - must be done before endpoints due to foreign key
echo "Fetching models..." >&2
MODELS_RESPONSE=$(curl -s -X GET "http://localhost:3001/admin/api/v1/models?limit=100" \
  -b "dwctl_session=${ADMIN_JWT}")

DELETED_MODELS=0
# Check if response has .data field (paginated response)
if echo "$MODELS_RESPONSE" | jq -e '.data' >/dev/null 2>&1; then
  # Paginated response
  MODELS=$(echo "$MODELS_RESPONSE" | jq -r '.data[]? | "\(.id):\(.alias)"')
  if [ -n "$MODELS" ]; then
    while IFS=: read -r model_id model_alias; do
      if [[ "$model_alias" =~ -permtest$ ]]; then
        echo "  Deleting model: $model_alias (ID: $model_id)" >&2
        if curl -s -o /dev/null -w "%{http_code}" -X DELETE "http://localhost:3001/admin/api/v1/models/${model_id}" \
          -b "dwctl_session=${ADMIN_JWT}" | grep -q "20[04]"; then
          ((DELETED_MODELS++))
        else
          echo "    ⚠️  Failed to delete model $model_alias" >&2
        fi
      fi
    done <<< "$MODELS"
  fi
else
  echo "  ⚠️  Unexpected models API response format" >&2
  echo "  Response: $MODELS_RESPONSE" >&2
fi

# Delete test endpoints
echo "Fetching endpoints..." >&2
ENDPOINTS_RESPONSE=$(curl -s -X GET "http://localhost:3001/admin/api/v1/endpoints?limit=100" \
  -b "dwctl_session=${ADMIN_JWT}")

DELETED_ENDPOINTS=0
# Check if response is an array (endpoints returns array directly, not paginated)
if echo "$ENDPOINTS_RESPONSE" | jq -e 'type == "array"' >/dev/null 2>&1; then
  # Direct array response
  ENDPOINTS=$(echo "$ENDPOINTS_RESPONSE" | jq -r '.[]? | "\(.id):\(.name)"')
  if [ -n "$ENDPOINTS" ]; then
    while IFS=: read -r endpoint_id endpoint_name; do
      if [[ "$endpoint_name" =~ -permtest$ ]]; then
        echo "  Deleting endpoint: $endpoint_name (ID: $endpoint_id)" >&2
        if curl -s -o /dev/null -w "%{http_code}" -X DELETE "http://localhost:3001/admin/api/v1/endpoints/${endpoint_id}" \
          -b "dwctl_session=${ADMIN_JWT}" | grep -q "20[04]"; then
          ((DELETED_ENDPOINTS++))
        else
          echo "    ⚠️  Failed to delete endpoint $endpoint_name" >&2
        fi
      fi
    done <<< "$ENDPOINTS"
  fi
else
  echo "  ⚠️  Unexpected endpoints API response format" >&2
  echo "  Response: $ENDPOINTS_RESPONSE" >&2
fi

echo "✅ Permission test resource cleanup complete (deleted $DELETED_FILES files, $DELETED_MODELS models, $DELETED_ENDPOINTS endpoints)" >&2