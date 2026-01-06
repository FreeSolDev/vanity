# Vanity Keypair Generator Service
# Runs solana-vanity in a container with a simple HTTP API

FROM rust:1.75-slim as builder

# Install solana-vanity
RUN cargo install solana-vanity

# Runtime image
FROM node:24-slim

# Copy solana-vanity binary from builder
COPY --from=builder /usr/local/cargo/bin/solana-vanity /usr/local/bin/solana-vanity

# Create app directory
WORKDIR /app

# Copy package files
COPY package*.json ./

# Install dependencies
RUN npm install --production

# Copy app source
COPY . .

# Expose port
EXPOSE 3000

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
  CMD curl -f http://localhost:3000/health || exit 1

CMD ["node", "server.js"]
