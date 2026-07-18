/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#pragma once

#include <stdbool.h>
#include <stdint.h>

#include "claw_agent.h"
#include "esp_err.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef struct {
    uint32_t session_id;
    uint32_t request_id;
} claw_im_session_input_t;

/* IM ingress helper: resolve channel+chat to the global AgentSystem ids, then
 * publish those ids with the text event. /session remains a control message
 * parsed by cap_agent and therefore does not create an implicit session. */
esp_err_t claw_im_session_publish_message(
    const char *source_cap,
    const char *channel,
    const char *chat_id,
    claw_agent_session_persistence_t persistence,
    const char *text,
    const char *sender_id,
    const char *message_id);

/* Resolve one inbound IM message before it is published to the event router.
 * A pending AgentSystem input request takes precedence; otherwise this returns
 * the chat's selected session, creating/opening one when needed. */
esp_err_t claw_im_session_prepare_input(
    const char *channel,
    const char *chat_id,
    claw_agent_session_persistence_t persistence,
    claw_im_session_input_t *out_input);

esp_err_t claw_im_session_get_selected(const char *channel,
                                       const char *chat_id,
                                       uint32_t *out_session_id);
esp_err_t claw_im_session_select(const char *channel,
                                 const char *chat_id,
                                 uint32_t session_id);

/* True only for sessions referenced by an IM chat cursor or pending IM input
 * request. cap_agent uses this to avoid taking over streams opened by another
 * C API consumer. */
bool claw_im_session_is_managed(uint32_t session_id);

esp_err_t claw_im_session_mark_open(uint32_t session_id);
esp_err_t claw_im_session_mark_closed(uint32_t session_id);
esp_err_t claw_im_session_forget(uint32_t session_id);

esp_err_t claw_im_session_note_input_request(const char *channel,
                                             const char *chat_id,
                                             uint32_t session_id,
                                             uint32_t request_id);
esp_err_t claw_im_session_clear_input_request(uint32_t session_id,
                                              uint32_t request_id);
esp_err_t claw_im_session_clear_session_input(uint32_t session_id);

#ifdef __cplusplus
}
#endif
