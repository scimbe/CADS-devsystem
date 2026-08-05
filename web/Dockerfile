# devsystem-web: the real control panel backend (#382). Built from this repo's
# root so it can pull in ../pipeline (a path dependency) and ../runs (the actual
# git-tracked run data every run lives in).
FROM rust:1-slim AS builder
WORKDIR /work
COPY pipeline ./pipeline
COPY web ./web
# Real fix (2026-08-05): this RUN had no BuildKit cache mount at all, so every
# single build recompiled the entire dependency tree (ct-common, axum, tokio,
# reqwest, ...) from scratch, and BuildKit cached that whole multi-GB layer as
# a fresh, non-deduplicated blob every time -- the actual root cause of this
# host's recurring disk-fill-to-critical pattern this session (manually
# `docker builder prune`d back to healthy repeatedly; every one of those
# prunes reclaimed 14-25GB). Cache mounts for the cargo registry and target
# dir (same established pattern as the sibling CADS-a2a-demo/CADS-auction-demo
# repos' own Dockerfiles on this host) let cargo actually reuse unchanged
# dependency artifacts across builds instead of rebuilding and re-caching them
# every time -- a cache mount's contents don't persist into the final image on
# their own, so the compiled binary is copied out to a normal path (/tmp)
# inside the same RUN step, before the mount disappears. `--jobs 1`: this
# host has zero swap and has frozen before under concurrent heavy builds
# (confirmed by CADS-a2a-demo's own Dockerfile comment, same host, same
# constraint) -- not worth risking here for a small speed gain.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/work/target \
    cargo build --release --manifest-path web/Cargo.toml --jobs 1 \
    && cp web/target/release/devsystem-web /tmp/devsystem-web

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /tmp/devsystem-web /app/devsystem-web
COPY web/static /app/web/static
ENV DEVSYSTEM_STATIC_DIR=/app/web/static
ENV DEVSYSTEM_RUNS_DIR=/app/runs
EXPOSE 8790
CMD ["/app/devsystem-web"]
