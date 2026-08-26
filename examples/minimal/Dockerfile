FROM debian:bookworm-slim

ARG CLANKERVM_SERVER_VERSION=0.1.0
ARG TARGETARCH
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl xz-utils \
    && case "$TARGETARCH" in \
        amd64) target=x86_64-unknown-linux-gnu ;; \
        arm64) target=aarch64-unknown-linux-gnu ;; \
        *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && archive="clankervm-server-${target}.tar.xz" \
    && release_url="https://github.com/jcarver989/clankervm/releases/download/clankervm-server-v${CLANKERVM_SERVER_VERSION}" \
    && curl --fail --location --silent --show-error --output "$archive" "$release_url/$archive" \
    && curl --fail --location --silent --show-error --output "$archive.sha256" "$release_url/$archive.sha256" \
    && sha256sum --check "$archive.sha256" \
    && directory="${archive%.tar.xz}" \
    && tar --extract --file "$archive" --xz \
    && install --mode 0755 "$directory/clankervm-server" /usr/local/bin/clankervm-server \
    && rm --recursive --force "$archive" "$archive.sha256" "$directory" \
    && apt-get purge --yes --auto-remove curl xz-utils \
    && rm --recursive --force /var/lib/apt/lists/*

CMD ["clankervm-server"]
