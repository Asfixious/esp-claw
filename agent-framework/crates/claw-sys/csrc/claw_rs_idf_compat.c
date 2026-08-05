/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */

/*
 * Compile-time guard for the ESP-IDF ABI consumed by claw-sys's handwritten
 * Rust FFI. Keep this translation unit in every firmware/test component that
 * links claw-sys so an unsupported SDK fails at build time instead of passing
 * a misaligned esp_http_client_config_t to esp_http_client_init().
 */
#include <stddef.h>

#include "esp_http_client.h"
#include "esp_idf_version.h"

#if ESP_IDF_VERSION_MAJOR != 6 || ESP_IDF_VERSION_MINOR != 1
#error "claw-sys supports ESP-IDF v6.1.x only"
#endif

/* All ESP-IDF targets supported by this repository use a 32-bit C ABI. */
_Static_assert(sizeof(void *) == 4, "claw-sys requires 32-bit ESP-IDF pointers");

/*
 * These offsets mirror esp_http_client_config_t in claw-sys/src/http.rs.
 * Checking the fields Rust writes catches both unconditional upstream fields
 * and Kconfig-dependent fields inserted anywhere in the consumed prefix.
 */
_Static_assert(offsetof(esp_http_client_config_t, url) == 0,
               "ESP-IDF HTTP ABI changed before url");
_Static_assert(offsetof(esp_http_client_config_t, client_key) == 56,
               "ESP-IDF HTTP ABI changed before client_key");
_Static_assert(offsetof(esp_http_client_config_t, client_key_password) == 60,
               "ESP-IDF HTTP ABI changed before client_key_password");
_Static_assert(offsetof(esp_http_client_config_t, event_handler) == 96,
               "ESP-IDF HTTP ABI changed before event_handler");
_Static_assert(offsetof(esp_http_client_config_t, transport_type) == 100,
               "ESP-IDF HTTP ABI changed before transport_type");
_Static_assert(offsetof(esp_http_client_config_t, buffer_size) == 104,
               "ESP-IDF HTTP ABI changed before buffer_size");
_Static_assert(offsetof(esp_http_client_config_t, buffer_size_tx) == 108,
               "ESP-IDF HTTP ABI changed before buffer_size_tx");
_Static_assert(offsetof(esp_http_client_config_t, user_data) == 112,
               "ESP-IDF HTTP ABI changed before user_data");
_Static_assert(offsetof(esp_http_client_config_t, is_async) == 116,
               "ESP-IDF HTTP ABI changed before is_async");
_Static_assert(offsetof(esp_http_client_config_t, crt_bundle_attach) == 124,
               "ESP-IDF HTTP ABI changed before crt_bundle_attach");
_Static_assert(offsetof(esp_http_client_config_t, keep_alive_enable) == 128,
               "ESP-IDF HTTP ABI changed before keep_alive_enable");

/* Rust reserves 208 bytes, including zero-filled space for IDF's tail fields. */
_Static_assert(sizeof(esp_http_client_config_t) <= 208,
               "ESP-IDF HTTP config no longer fits claw-sys's reserved storage");
