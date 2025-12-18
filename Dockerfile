# MCP Test Server Dockerfile
# Lightweight Node.js service for testing MCP tools via HTTP

FROM node:20-alpine

WORKDIR /app

# Copy package files
COPY package*.json ./

# Install dependencies (production only)
RUN npm ci --production

# Copy built files
COPY dist/ ./dist/

# Expose the test server port
EXPOSE 3099

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
  CMD wget --no-verbose --tries=1 --spider http://localhost:3099/health || exit 1

# Run the test server
ENV NODE_ENV=production
ENV MCP_TEST_PORT=3099

CMD ["node", "dist/test-server.js"]
