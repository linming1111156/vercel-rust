FROM rust:1.99-slim AS build
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:stable-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates procps net-tools bash \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=build /app/target/release/nzws-rust /app/nzws-rust
COPY index.html /app/index.html
EXPOSE 3000
CMD ["/app/nzws-rust"]
