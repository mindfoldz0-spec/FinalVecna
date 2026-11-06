# Stage 0: build the React frontend
FROM node:20-bookworm-slim AS web
WORKDIR /web
COPY frontend/package.json frontend/package-lock.json* ./
RUN npm install --no-audit --no-fund 2>&1 | tail -1 || npm install --no-audit --no-fund
COPY frontend ./
RUN npm run build && ls dist/index.html

# Stage 1: build the Rust backend
FROM rust:1.90-bookworm AS build
WORKDIR /app
COPY backend/Cargo.toml backend/Cargo.lock ./backend/
COPY backend/src ./backend/src
COPY backend/migrations ./backend/migrations
WORKDIR /app/backend
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/backend/target/release/vecna-server /usr/local/bin/vecna-server
COPY --from=web /web/dist /app/frontend/dist
WORKDIR /app/backend
EXPOSE 8000
CMD ["vecna-server"]
