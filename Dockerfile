# One image with every binary of the workspace (collector, later api).
# Queries are checked against the committed .sqlx/ directory, so no database
# is needed at build time.

FROM rust:1.99-slim-trixie AS build
WORKDIR /src
ENV SQLX_OFFLINE=true
COPY . .
RUN cargo build --release --locked --workspace \
 && mkdir /out \
 && cp target/release/collector /out/

FROM debian:trixie-slim
# CA certificates for HTTPS to the Blizzard API.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --no-create-home --uid 10001 app
COPY --from=build /out/ /usr/local/bin/
USER app
CMD ["collector"]
