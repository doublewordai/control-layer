#!/bin/bash

# Extract admin credentials from config.yaml
ADMIN_EMAIL=$(grep 'admin_email:' config.yaml | sed 's/.*admin_email:[ ]*"\(.*\)"/\1/')
ADMIN_PASSWORD=$(grep 'admin_password:' config.yaml | sed 's/.*admin_password:[ ]*"\(.*\)"/\1/')

if [ -z "$ADMIN_EMAIL" ]; then
  echo "Failed to extract admin email from config.yaml" >&2
  exit 1
fi

if [ -z "$ADMIN_PASSWORD" ]; then
  echo "Failed to extract admin password from config.yaml" >&2
  exit 1
fi

# Generate admin JWT for authentication
ADMIN_JWT=$(EMAIL=$ADMIN_EMAIL PASSWORD=$ADMIN_PASSWORD ./scripts/login.sh 2>/dev/null)

if [ -z "$ADMIN_JWT" ]; then
  echo "Failed to generate admin JWT" >&2
  exit 1
fi

echo "Fetching all users..." >&2

# Get all users
USERS=$(curl -s -X GET http://localhost:3001/admin/api/v1/users \
  -b "dwctl_session=${ADMIN_JWT}" | jq -r '.[] | "\(.id):\(.email)"')

if [ -z "$USERS" ]; then
  echo "No users found or failed to fetch users" >&2
  exit 1
fi

# Count of deleted users
DELETED_COUNT=0

# Process each user
while IFS=: read -r user_id user_email; do
  # Check if this user matches the test user patterns (test-*@example.org or user@example.org)
  if [[ "$user_email" =~ ^test-.*@example\.org$ ]] || [[ "$user_email" == "user@example.org" ]]; then
    echo "Deleting test user: $user_email (ID: $user_id)" >&2

    # Delete the user
    HTTP_STATUS=$(curl -s -o /dev/null -w "%{http_code}" \
      -X DELETE "http://localhost:3001/admin/api/v1/users/${user_id}" \
      -b "dwctl_session=${ADMIN_JWT}")

    if [ "$HTTP_STATUS" = "204" ]; then
      echo "✅ Successfully deleted $user_email" >&2
      ((DELETED_COUNT++))
    else
      echo "❌ Failed to delete $user_email (HTTP $HTTP_STATUS)" >&2
    fi
  fi
done <<<"$USERS"

echo "" >&2
echo "Deleted $DELETED_COUNT test user(s)" >&2
