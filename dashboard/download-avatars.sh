#!/bin/bash

# Download random avatars for users
echo "Downloading random avatars..."

for i in {1..6}; do
  curl -s "https://avatar.iran.liara.run/public" -o "public/avatars/user-${i}.png"
  echo "Downloaded user-${i}.png"
  sleep 1  # Small delay to ensure different random avatars
done

echo "All avatars downloaded!"