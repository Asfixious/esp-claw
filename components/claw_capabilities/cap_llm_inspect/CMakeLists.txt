# cap_llm_inspect.c (the inspect_image capability that runs LLM media
# inference) is now implemented in Rust (../../claw_modules/rust/cap_llm_inspect)
# and bundled into libclaw_rt.a via the claw_core component. The console command
# (cmd_cap_llm_inspect.c) stays in C; it only uses esp_console/argtable3/claw_cap
# and reaches the capability through claw_cap_call. cap_llm_inspect_register_group
# resolves from libclaw_rt.a through the claw_core dependency.
idf_component_register(
    SRCS
        "src/cmd_cap_llm_inspect.c"
    INCLUDE_DIRS
        "include"
    REQUIRES
        claw_cap
        claw_core
        console
)
