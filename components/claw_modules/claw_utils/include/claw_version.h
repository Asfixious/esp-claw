/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#pragma once

#ifdef __cplusplus
extern "C" {
#endif

#define CLAW_VERSION_MAJOR 0
#define CLAW_VERSION_MINOR 1
#define CLAW_VERSION_PATCH 0
#define CLAW_VERSION_VAL(major, minor, patch) (((major) << 16) | ((minor) << 8) | (patch))
#define CLAW_VERSION CLAW_VERSION_VAL(CLAW_VERSION_MAJOR, CLAW_VERSION_MINOR, CLAW_VERSION_PATCH)

const char *claw_get_version(void);
const char *claw_get_git_version(void);

#ifdef __cplusplus
}
#endif
