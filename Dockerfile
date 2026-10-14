FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ffmpeg ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/fpv-catalog-bot /usr/local/bin/fpv-catalog-bot
USER 10001:10001
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/fpv-catalog-bot"]
