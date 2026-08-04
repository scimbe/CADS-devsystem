# devsystem-web: the real control panel backend (#382). Built from this repo's
# root so it can pull in ../pipeline (a path dependency) and ../runs (the actual
# git-tracked run data every run lives in).
FROM rust:1-slim AS builder
WORKDIR /work
COPY pipeline ./pipeline
COPY web ./web
RUN cargo build --release --manifest-path web/Cargo.toml

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /work/web/target/release/devsystem-web /app/devsystem-web
COPY web/static /app/web/static
ENV DEVSYSTEM_STATIC_DIR=/app/web/static
ENV DEVSYSTEM_RUNS_DIR=/app/runs
EXPOSE 8790
CMD ["/app/devsystem-web"]
