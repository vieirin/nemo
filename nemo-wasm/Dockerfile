# syntax=docker/dockerfile:1
#
# Reproducible build of the nemo-wasm packages, without a local Rust toolchain.
# Run from the repository root:
#
#   docker build -f nemo-wasm/Dockerfile --output type=local,dest=nemo-wasm .
#
# writes `nemo-wasm/pkg-node` (`--target nodejs`) and `nemo-wasm/pkg-web`
# (`--target web`), both built without the language server. Pass
# `--build-arg FEATURES=language-server` to include it.
#
# The `toolchain` stage can also be used to run the tests:
#
#   docker build -f nemo-wasm/Dockerfile --target toolchain -t nemo-toolchain .
#   docker run --rm -v "$PWD":/nemo -v nemo-target:/nemo/target \
#     -v nemo-cargo:/usr/local/cargo/registry nemo-toolchain cargo test -p nemo

FROM rust:1-bookworm AS toolchain
# `rust-toolchain.toml` asks for an undated nightly (`nemo-wasm` needs
# `#![feature(alloc_error_hook)]`); pin a dated one here so builds are repeatable.
ARG NIGHTLY=nightly-2026-10-12
ARG WASM_PACK_VERSION=0.13.1
ENV RUSTUP_TOOLCHAIN=${NIGHTLY}
RUN rustup toolchain install ${NIGHTLY} --profile minimal \
      --target wasm32-unknown-unknown --component rustfmt,clippy
RUN cargo install wasm-pack --locked --version ${WASM_PACK_VERSION}
WORKDIR /nemo

FROM toolchain AS build
ARG FEATURES=""
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/nemo/target \
    cd nemo-wasm \
 && for target in nodejs web; do \
      case $target in nodejs) out=/out/pkg-node ;; *) out=/out/pkg-web ;; esac; \
      wasm-pack build --release --weak-refs --target $target --out-dir $out \
        -- --no-default-features --features "web_sys_unstable_apis ${FEATURES}" || exit 1; \
    done

FROM scratch
COPY --from=build /out /
