#FROM rust:1.88-slim
#FROM rust:nightly-trixie-slim
FROM rust:trixie as chef

ENV DEBIAN_FRONTEND=noninteractive

WORKDIR /app

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl && \
    rm -rf /var/lib/apt/lists/*
    
RUN cargo install cargo-chef --locked

FROM chef as planner
COPY Cargo.toml Cargo.lock ./
RUN cargo chef prepare --recipe-path recipe.json

FROM chef as builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

COPY src/ src/

RUN cargo build --release
RUN install -Dm755 target/release/finguard_rs_backend /usr/local/bin/finguard-rs

COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh


FROM debian:trixie-slim AS runtime

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates gosu && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/bin/finguard-rs /usr/local/bin/
COPY --from=builder /usr/local/bin/entrypoint.sh /usr/local/bin/

ENV XDG_DATA_HOME=/data \
    XDG_CONFIG_HOME=/config \
    FINGUARD_HOST=0.0.0.0 \
    FINGUARD_PORT=3111

EXPOSE 3111

ENTRYPOINT ["sh", "/usr/local/bin/entrypoint.sh"]
CMD ["finguard-rs", "--host", "0.0.0.0", "--port", "3111"]
