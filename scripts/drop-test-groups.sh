#!/bin/bash

# Extract admin email from config.yaml
ADMIN_EMAIL=$(grep 'admin_email:' config.yaml | sed 's/.*admin_email:[ ]*"\(.*\)"/\1/')

if [ -z "$ADMIN_EMAIL" ]; then
  echo "Failed to extract admin email from config.yaml" >&2
  exit 1
fi

# These scripts run only against the local isolated HTTP-test stack, where
# the harness supplies the trusted SSO headers for the configured admin.
echo "Fetching all groups..." >&2

# Get all groups (API returns paginated response, use max limit of 100)
TEST_GROUP_ROWS=$(curl -s -X GET "http://localhost:3001/admin/api/v1/groups?limit=100" \
  -H "X-Doubleword-User: ${ADMIN_EMAIL}" -H "X-Doubleword-Email: ${ADMIN_EMAIL}" | jq -r '.data[] | "\(.id):\(.name)"')

if [ -z "$TEST_GROUP_ROWS" ]; then
  echo "No groups found or failed to fetch groups" >&2
  exit 0
fi

# Count of deleted groups
DELETED_COUNT=0

# Process each group
while IFS=: read -r group_id group_name; do
  # Check if this group matches the test group pattern (test-group-*)
  if [[ "$group_name" =~ ^test-group-.* ]]; then
    echo "Deleting test group: $group_name (ID: $group_id)" >&2

    # Delete the group
    HTTP_STATUS=$(curl -s -o /dev/null -w "%{http_code}" \
      -X DELETE "http://localhost:3001/admin/api/v1/groups/${group_id}" \
      -H "X-Doubleword-User: ${ADMIN_EMAIL}" -H "X-Doubleword-Email: ${ADMIN_EMAIL}")

    if [ "$HTTP_STATUS" = "204" ]; then
      echo "✅ Successfully deleted $group_name" >&2
      ((DELETED_COUNT++))
    else
      echo "❌ Failed to delete $group_name (HTTP $HTTP_STATUS)" >&2
    fi
  fi
done <<<"$TEST_GROUP_ROWS"

echo "" >&2
echo "Deleted $DELETED_COUNT test group(s)" >&2
