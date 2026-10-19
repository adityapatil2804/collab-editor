# Multi-stage build: Rust server + compiled web client in a small runtime image.
FROM rust:1-bookworm AS server
WORKDIR /src
COPY Cargo.toml Cargo.lock* ./
COPY crates ./crates
RUN cargo build --release -p collab-server

FROM node:22-bookworm-slim AS web
WORKDIR /web
COPY web/package.json web/tsconfig.json ./
RUN npm install --no-audit --no-fund
COPY web/src ./src
RUN npx tsc -p .

FROM debian:bookworm-slim
COPY --from=server /src/target/release/collab-server /usr/local/bin/collab-server
COPY web/index.html web/style.css web/sw.js web/manifest.webmanifest web/icon.svg /srv/web/
COPY --from=web /web/dist /srv/web/dist
VOLUME /data
EXPOSE 8080
CMD ["collab-server", "--port", "8080", "--data", "/data", "--static", "/srv/web"]
