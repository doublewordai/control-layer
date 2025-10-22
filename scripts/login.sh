#!/bin/bash

# Check if email and password are provided via environment variables
if [ -z "$EMAIL" ]; then
  echo "EMAIL environment variable not set" >&2
  echo "Usage: EMAIL=user@example.com PASSWORD=yourpassword $0" >&2
  exit 1
fi

if [ -z "$PASSWORD" ]; then
  echo "PASSWORD environment variable not set" >&2
  echo "Usage: EMAIL=user@example.com PASSWORD=yourpassword $0" >&2
  exit 1
fi

# Call the login endpoint and capture the cookie
echo "Attempting login with email: $EMAIL" >&2
RESPONSE=$(curl -s -c - -w "\nHTTP_STATUS:%{http_code}" \
  -X POST \
  -H "Content-Type: application/json" \
  -d "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}" \
  http://localhost:3001/authentication/login 2>/dev/null)

HTTP_STATUS=$(echo "$RESPONSE" | sed -n 's/.*HTTP_STATUS:\([0-9]*\).*/\1/p')

if [ "$HTTP_STATUS" = "200" ]; then
  # Extract the cookie value from the response
  COOKIE=$(echo "$RESPONSE" | awk '/dwctl_session/ {for(i=1;i<=NF;i++) if($i=="dwctl_session") print $(i+1); exit}')
  if [ -n "$COOKIE" ]; then
    echo "$COOKIE"
  else
    echo "❌ No cookie found in response" >&2
    echo "Response:" >&2
    echo "$RESPONSE" >&2
    exit 1
  fi
else
  echo "❌ Login failed for $EMAIL (HTTP $HTTP_STATUS)" >&2
  echo "Response body:" >&2
  echo "$RESPONSE" | grep -v "HTTP_STATUS:" >&2
  exit 1
fi
