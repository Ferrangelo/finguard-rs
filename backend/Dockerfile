#FROM rust:1.88-slim
#FROM rust:nightly-trixie-slim
FROM rust:trixie

ENV DEBIAN_FRONTEND=noninteractive
ENV RUSTFLAGS="-C target-cpu=native"

WORKDIR /app

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl gosu apt-utils && \
    rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./

# Cache dependencies
RUN mkdir -p src/ \
    && echo "fn main(){}" > src/main.rs
RUN cargo build --release

COPY src/ src/

RUN cargo build --release && \
    cp target/release/finguard_rs_backend /usr/local/bin/finguard-rs && \
    rm -rf target

COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

ENV XDG_DATA_HOME=/data \
    XDG_CONFIG_HOME=/config \
    FINGUARD_HOST=0.0.0.0 \
    FINGUARD_PORT=3111

EXPOSE 3111

ENTRYPOINT ["sh", "/usr/local/bin/entrypoint.sh"]
#CMD ["finguard-rs"]
CMD ["finguard-rs", "--host", "0.0.0.0", "--port", "3111"]
