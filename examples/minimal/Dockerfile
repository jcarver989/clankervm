FROM rust:1.97-bookworm AS build
ARG CLANKERVM_SERVER_VERSION=0.1.0
RUN cargo install --locked --version "$CLANKERVM_SERVER_VERSION" clankervm-server

FROM debian:bookworm-slim
COPY --from=build /usr/local/cargo/bin/clankervm-server /usr/local/bin/
CMD ["clankervm-server"]
