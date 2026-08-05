idf_component_register(
    SRCS
        "src/lua_module_ir.c"
    INCLUDE_DIRS
        "src"
    REQUIRES
        cap_lua
        esp_board_manager
        esp_driver_rmt
        esp_driver_gpio
)

# ir_learn still declares the legacy "driver" component, so its own sources do
# not inherit the split RMT driver's headers in IDF 6.1. Keep the workaround at
# the consumer until the managed component declares esp_driver_rmt itself.
idf_component_get_property(ir_learn_lib espressif__ir_learn COMPONENT_LIB)
idf_component_get_property(rmt_lib esp_driver_rmt COMPONENT_LIB)
target_link_libraries(${ir_learn_lib} PRIVATE ${rmt_lib})

idf_component_set_property(${COMPONENT_NAME} WHOLE_ARCHIVE TRUE)
