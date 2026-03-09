FROM node:24-alpine

RUN npm install -g pnpm@10

WORKDIR /app

# Copy package files
COPY package.json pnpm-lock.yaml ./

# Install dependencies
RUN pnpm install --frozen-lockfile

# Expose the Vite dev server port
EXPOSE 5173

# Install dependencies and run the dev server
CMD ["sh", "-c", "pnpm run dev -- --host 0.0.0.0"]
