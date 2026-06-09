# claw_memory is now implemented in Rust (../rust/claw_memory) and bundled into
# libclaw_rt.a via the claw_core component. This component is header-only: it
# exposes claw_memory.h, and all claw_memory_* symbols resolve from libclaw_rt.a
# through the claw_core dependency.
idf_component_register(
    INCLUDE_DIRS
        "include"
    REQUIRES
        claw_cap
        claw_core
)
