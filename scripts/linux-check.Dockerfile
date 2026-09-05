# Keep this version aligned with rust-toolchain.toml. The digest fixes the
# multi-platform base image while allowing native ARM64 and x86-64 execution.
FROM rust:1.97.1-bookworm@sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97

RUN apt-get update \
    && apt-get install --yes --no-install-recommends zsh locales procps \
    && rm -rf /var/lib/apt/lists/* \
    && localedef -i en_US -f UTF-8 en_US.UTF-8 \
    && rustup component add clippy rustfmt \
    && useradd --create-home --uid 1000 quirl \
    && mkdir -p /home/quirl/.cargo /home/quirl/target \
    && chown -R quirl:quirl /home/quirl

# Run permission and persistence tests as an ordinary user. Keep Linux build
# artifacts separate from the host target directory and source mount.
ENV CARGO_HOME=/home/quirl/.cargo \
    CARGO_TARGET_DIR=/home/quirl/target \
    CARGO_BUILD_JOBS=2 \
    RUST_TEST_THREADS=2 \
    LANG=en_US.UTF-8
USER quirl
RUN git config --global safe.directory /workspace
WORKDIR /workspace
CMD ["cargo", "xtask", "check"]
