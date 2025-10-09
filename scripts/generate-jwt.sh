#!/bin/bash

# Check if username and password are provided via environment variables
if [ -z "$USERNAME" ]; then
  echo "USERNAME environment variable not set" >&2
  echo "Usage: USERNAME=user@example.com PASSWORD=yourpassword $0" >&2
  exit 1
fi

if [ -z "$PASSWORD" ]; then
  echo "PASSWORD environment variable not set" >&2
  echo "Usage: USERNAME=user@example.com PASSWORD=yourpassword $0" >&2
  exit 1
fi

# Call the login endpoint and capture the cookie
RESPONSE=$(curl -s -c - -w "\nHTTP_STATUS:%{http_code}" \
  -X POST \
  -H "Content-Type: application/json" \
  -d "{\"email\":\"$USERNAME\",\"password\":\"$PASSWORD\"}" \
  http://localhost:3001/authentication/login 2>/dev/null)

HTTP_STATUS=$(echo "$RESPONSE" | grep -oP 'HTTP_STATUS:\K\d+')

if [ "$HTTP_STATUS" = "200" ]; then
  # Extract the cookie value from the response
  COOKIE=$(echo "$RESPONSE" | grep -oP 'clay_session\s+\K[^\s]+' | head -1)
  if [ -n "$COOKIE" ]; then
    echo "$COOKIE"
  else
    echo "❌ No cookie found in response" >&2
    echo "Response:" >&2
    echo "$RESPONSE" >&2
    exit 1
  fi
else
  echo "❌ Login failed for $USERNAME (HTTP $HTTP_STATUS)" >&2
  echo "Response body:" >&2
  echo "$RESPONSE" | grep -v "HTTP_STATUS:" >&2
  exit 1
fi
