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
# #48 slice 6: also build the real github_issue_channel_client binary into this
# same image -- approve_issue_proposal shells out to it (as a subprocess, same
# process this container already runs) to relay an approved issue through the
# separate, isolated github_issue_channel_handler agent instead of POSTing to
# GitHub directly. Built from the same pipeline/ checkout, same cache mounts,
# in the one cargo invocation so this doesn't double the dependency-compile cost.
#
# Real bug found and fixed (2026-08-05, same day as the cache mount above was
# added): the cache mount below targets /work/target, but neither `cargo build
# --manifest-path web/Cargo.toml` nor `--manifest-path pipeline/Cargo.toml`
# writes there by default -- each defaults to <manifest-dir>/target
# (/work/web/target, /work/pipeline/target), confirmed directly against the
# cp commands below, which have always read from exactly those paths, never
# from /work/target. The mount was caching an empty, unused directory --
# compiled artifacts (the expensive part, all the real rustc invocation time)
# were never actually persisted across builds, only the registry cache mount
# (crate source downloads) was doing anything. CARGO_TARGET_DIR redirects both
# cargo invocations' real output to the one directory the mount actually is.
ENV CARGO_TARGET_DIR=/work/target
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/work/target \
    cargo build --release --manifest-path web/Cargo.toml --jobs 1 \
    && cargo build --release --manifest-path pipeline/Cargo.toml --jobs 1 --bin github_issue_channel_client \
    && cp /work/target/release/devsystem-web /tmp/devsystem-web \
    && cp /work/target/release/github_issue_channel_client /tmp/github_issue_channel_client

# Real fix (2026-08-05): rust:1-slim above now tracks Debian 13 (trixie,
# glibc 2.41) upstream, not bookworm -- discovered the hard way when this
# stage was still `debian:bookworm-slim` (Debian 12, glibc 2.36) and the
# resulting binary refused to start at all: "GLIBC_2.39 not found". Runtime
# base has to track whatever Debian release the `rust:1-slim` builder tag
# currently resolves to, not a version pinned when this Dockerfile was first
# written.
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /tmp/devsystem-web /app/devsystem-web
COPY --from=builder /tmp/github_issue_channel_client /app/github_issue_channel_client
COPY web/static /app/web/static
ENV DEVSYSTEM_STATIC_DIR=/app/web/static
ENV DEVSYSTEM_RUNS_DIR=/app/runs
# #48 slice 6: where approve_issue_proposal finds its own copy of the client
# subprocess, baked into this same image -- CT_AGENT_BIN (the real ct-agent
# binary, a different architecture-sensitive artifact this repo doesn't build)
# and the channel/key material are deliberately NOT set here: those are real,
# host-specific/secret values the operator supplies at `docker run` time (see
# scripts/github_issue_agent_serve_loop.sh's own doc comment for the matching
# handler-side contract), same as this image never bakes in DEVSYSTEM_GITHUB_TOKEN.
ENV ISSUE_CHANNEL_CLIENT_BIN=/app/github_issue_channel_client
EXPOSE 8790
CMD ["/app/devsystem-web"]
