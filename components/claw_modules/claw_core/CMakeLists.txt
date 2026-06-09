# claw_core is implemented in Rust (see ../rust/claw_core and its workspace
# crates). Several migrated claw_modules (claw_core, claw_memory,
# cap_llm_inspect, claw_paths, claw_task) are Rust crates that share upstream
# code; building each as its own staticlib would emit the shared objects (and
# the #[no_mangle] C-ABI symbols) into multiple archives and fail the final
# link with duplicate symbols. They are therefore bundled into ONE static
# archive (libclaw_rt.a) by the `claw_rt` aggregator crate, which this single
# ESP-IDF component links. The claw_memory and cap_llm_inspect components are
# header-only and depend on claw_core for the symbols.
#
# The only C source still compiled here is the tiny log shim that lets the Rust
# `log` backend reach `esp_log_write` (csrc lives next to the claw_sys crate).
#
# NOTE: building this component requires the Espressif Rust toolchain
# (`espup install`, channel "esp" pinned in ../rust/rust-toolchain.toml) on PATH.

idf_component_register(
    SRCS
        "../rust/claw_sys/csrc/claw_rs_log.c"
    INCLUDE_DIRS
        "include"
    REQUIRES
        esp_http_client
        http_reuse
        esp-tls
        freertos
        mbedtls
    PRIV_REQUIRES
        claw_event_router
    # The Rust log backend in libclaw_rt.a references `claw_rs_log` (the C shim
    # compiled into this component's archive). WHOLE_ARCHIVE guarantees the shim
    # object is always linked, independent of static-archive scan order.
    WHOLE_ARCHIVE
)

set(claw_core_rust_dir "${CMAKE_CURRENT_SOURCE_DIR}/../rust")
set(claw_core_rust_target "xtensa-esp32s3-espidf")
set(claw_core_rust_profile "release")
set(claw_core_rust_lib
    "${claw_core_rust_dir}/target/${claw_core_rust_target}/${claw_core_rust_profile}/libclaw_rt.a")

# Always invoke cargo; it performs its own incremental rebuild and only rewrites
# the archive when the Rust sources change.
add_custom_target(claw_core_rust_build
    BYPRODUCTS "${claw_core_rust_lib}"
    # `std` is not shipped precompiled for *-espidf, so build it from source.
    # The `esp` toolchain (pinned in ../rust/rust-toolchain.toml) accepts -Z.
    COMMAND cargo build --${claw_core_rust_profile} -p claw_rt
            --target ${claw_core_rust_target} -Z build-std=std,panic_abort
    WORKING_DIRECTORY "${claw_core_rust_dir}"
    COMMENT "Building claw_rt Rust staticlib (${claw_core_rust_target})"
    VERBATIM
    USES_TERMINAL
)

add_library(claw_core_rust STATIC IMPORTED GLOBAL)
set_target_properties(claw_core_rust PROPERTIES
    IMPORTED_LOCATION "${claw_core_rust_lib}")
add_dependencies(claw_core_rust claw_core_rust_build)

# Link the Rust archive into the component and make sure it is built first.
target_link_libraries(${COMPONENT_LIB} PUBLIC claw_core_rust)
add_dependencies(${COMPONENT_LIB} claw_core_rust_build)
