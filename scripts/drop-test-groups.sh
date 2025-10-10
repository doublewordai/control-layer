#!/bin/bash

# Extract admin email from clay_config.yaml
ADMIN_EMAIL=$(grep 'admin_email:' clay_config.yaml | sed 's/.*admin_email:[ ]*"\(.*\)"/\1/')

if [ -z "$ADMIN_EMAIL" ]; then
  echo "Failed to extract admin email from clay_config.yaml" >&2
  exit 1
fi

# Check for admin password
if [ -z "$ADMIN_PASSWORD" ]; then
  echo "ADMIN_PASSWORD environment variable not set" >&2
  exit 1
fi

# Generate admin cookie for authentication
ADMIN_JWT=$(EMAIL="$ADMIN_EMAIL" PASSWORD="$ADMIN_PASSWORD" ./scripts/generate-jwt.sh)

if [ -z "$ADMIN_JWT" ]; then
  echo "Failed to generate admin JWT" >&2
  exit 1
fi

echo "Fetching all groups..." >&2

# Get all groups
GROUPS=$(curl -s -X GET https://localhost/admin/api/v1/groups \
  -b "VouchCookie=${ADMIN_JWT}" | jq -r '.[] | "\(.id):\(.name)"')

if [ -z "$GROUPS" ]; then
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
      -X DELETE "https://localhost/admin/api/v1/groups/${group_id}" \
      -b "VouchCookie=${ADMIN_JWT}")

    if [ "$HTTP_STATUS" = "204" ]; then
      echo "✅ Successfully deleted $group_name" >&2
      ((DELETED_COUNT++))
    else
      echo "❌ Failed to delete $group_name (HTTP $HTTP_STATUS)" >&2
    fi
  fi
done <<<"$GROUPS"

echo "" >&2
echo "Deleted $DELETED_COUNT test group(s)" >&2