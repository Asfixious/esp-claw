# syntax=docker/dockerfile:1

ARG IDF_IMAGE=espressif/idf:release-v6.1
FROM ${IDF_IMAGE}

ARG RUST_VERSION=1.96.0
ARG ESPUP_VERSION=0.17.1
ARG ESP_RUST_TOOLCHAIN_VERSION=1.95.0.0
ARG IDF_BUILD_APPS_VERSION=3.0.2
ARG ESP_BMGR_ASSIST_VERSION=0.8.3

USER root
SHELL ["/bin/bash", "-o", "pipefail", "-c"]

ENV RUSTUP_HOME=/opt/rust/rustup \
    CARGO_HOME=/opt/rust/cargo \
    ESPUP_EXPORT_FILE=/opt/rust/export-esp.sh \
    PATH=/opt/rust/cargo/bin:${PATH}

# The upstream IDF image already provides curl, build-essential, and the IDF
# cross-compilers. espup additionally requires pkg-config on Debian/Ubuntu.
RUN apt-get update \
    && apt-get install --yes --no-install-recommends pkg-config \
    && rm -rf /var/lib/apt/lists/*

# --std skips espup's duplicate GCC installation. ESP-IDF supplies the C
# toolchains, while espup supplies the Rust/LLVM pieces and export file.
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain "${RUST_VERSION}" \
    && cargo install espup --version "${ESPUP_VERSION}" --locked \
    && espup install \
        --std \
        --stable-version "${RUST_VERSION}" \
        --toolchain-version "${ESP_RUST_TOOLCHAIN_VERSION}" \
        --targets esp32s3,esp32c5,esp32p4 \
        --export-file "${ESPUP_EXPORT_FILE}" \
    && test -s "${ESPUP_EXPORT_FILE}" \
    && rustc --version \
    && rustc +esp --version \
    && cargo +esp --version \
    && rm -rf "${CARGO_HOME}/registry" "${CARGO_HOME}/git"

# Install project-level Python build helpers into the IDF-managed virtualenv.
# Sourcing export.sh here selects the same Python environment used at runtime.
RUN source "${IDF_PATH}/export.sh" >/dev/null \
    && python -m pip install --no-cache-dir \
        "idf-build-apps==${IDF_BUILD_APPS_VERSION}" \
        "esp-bmgr-assist==${ESP_BMGR_ASSIST_VERSION}" \
    && python -c 'import idf_build_apps, esp_bmgr_py'

LABEL org.opencontainers.image.title="ESP-Claw ESP-IDF Rust CI" \
      org.opencontainers.image.description="ESP-IDF v6.1 with pinned Rust and firmware CI tools"
