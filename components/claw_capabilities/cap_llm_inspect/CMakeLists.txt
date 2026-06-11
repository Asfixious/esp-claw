# cap_llm_inspect implements the inspect_image capability in C. It reaches the
# agent core's LLM media inference through the claw_core_llm.h C ABI (exported
# from libclaw_rt.a via the claw_core component). The console command
# (cmd_cap_llm_inspect.c) only uses esp_console/argtable3/claw_cap.
idf_component_register(
    SRCS
        "src/cap_llm_inspect.c"
        "src/cmd_cap_llm_inspect.c"
    INCLUDE_DIRS
        "include"
    REQUIRES
        claw_cap
        claw_core
        console
)
