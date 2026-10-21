# Build the web UI, compile the gateway, ship one binary.

# 1. The web UI, which the gateway embeds at build time.
FROM node:22-bookworm-slim AS ui
WORKDIR /src/apps/ui
RUN corepack enable
COPY apps/ui/package.json apps/ui/pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile
COPY apps/ui/ ./
RUN pnpm build

# 2. The gateway binary. Same Debian release as the runtime image below.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
COPY --from=ui /src/apps/ui/dist apps/ui/dist
RUN cargo build --release --locked -p respan-gateway

# 3. Runtime: glibc, CA certificates, the binary, nothing else.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/respan-gateway /usr/local/bin/respan-gateway
# The gateway listens on 0.0.0.0:$PORT when PORT is set.
ENTRYPOINT ["/usr/local/bin/respan-gateway"]
