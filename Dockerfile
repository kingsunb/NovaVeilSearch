# syntax=docker/dockerfile:1
#
# nova-veil-search — Streamable HTTP MCP server with server-held credentials.
# Configure provider keys through the environment or the optional settings UI.

# ---- build stage ----------------------------------------------------------
FROM rust:1-bookworm AS builder
WORKDIR /app
COPY . .
# release-http => panic=unwind so a handler panic can't abort the whole process.
RUN cargo build --profile release-http --features http \
    && cp target/release-http/nova-veil-search /nova-veil-search

# ---- runtime stage --------------------------------------------------------
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --user-group --no-create-home --shell /usr/sbin/nologin grokmcp \
    && install -d -m 0700 -o grokmcp -g grokmcp /var/lib/nova-veil-search
COPY --from=builder /nova-veil-search /usr/local/bin/nova-veil-search
# Bind all interfaces inside the container; a reverse proxy terminates TLS.
ENV GROK_MCP_BIND=0.0.0.0:8080
ENV GROK_SEARCH_CONFIG=/var/lib/nova-veil-search/config.toml
EXPOSE 8080
USER grokmcp
ENTRYPOINT ["nova-veil-search", "--http"]
