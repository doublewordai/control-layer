FROM node:24-alpine

WORKDIR /app

# Copy package files
COPY package*.json ./

# Install dependencies
RUN npm ci

# Expose the Vite dev server port
EXPOSE 5173

# Install dependencies and run the dev server
CMD ["sh", "-c", "npm run dev -- --host 0.0.0.0"]
