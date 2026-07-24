/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#include "cap_llm_inspect_internal.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <sys/stat.h>

#include "mbedtls/base64.h"

static const char *image_mime_from_path(const char *path)
{
    const char *extension = NULL;

    if (!path || !path[0]) {
        return NULL;
    }

    extension = strrchr(path, '.');
    if (!extension) {
        return NULL;
    }
    if (strcasecmp(extension, ".jpg") == 0 || strcasecmp(extension, ".jpeg") == 0) {
        return "image/jpeg";
    }
    if (strcasecmp(extension, ".png") == 0) {
        return "image/png";
    }
    if (strcasecmp(extension, ".gif") == 0) {
        return "image/gif";
    }
    if (strcasecmp(extension, ".webp") == 0) {
        return "image/webp";
    }
    return NULL;
}

esp_err_t cap_llm_inspect_media_load(const char *path,
                                     size_t image_max_bytes,
                                     cap_llm_inspect_media_t *out_media,
                                     char **out_error_message)
{
    struct stat file_stat = {0};
    FILE *file = NULL;
    unsigned char *raw = NULL;
    unsigned char *encoded = NULL;
    const char *mime_type = NULL;
    size_t file_size;
    size_t encoded_capacity;
    size_t encoded_size = 0;
    size_t read_size;
    esp_err_t err = ESP_OK;

    if (out_media) {
        memset(out_media, 0, sizeof(*out_media));
    }
    if (out_error_message) {
        *out_error_message = NULL;
    }
    if (!path || !out_media || !out_error_message || image_max_bytes == 0) {
        return ESP_ERR_INVALID_ARG;
    }
    if (path[0] != '/') {
        *out_error_message = strdup("Image path must be absolute");
        return ESP_ERR_INVALID_ARG;
    }

    mime_type = image_mime_from_path(path);
    if (!mime_type) {
        *out_error_message = strdup("Only jpg, jpeg, png, gif, and webp images are supported");
        return ESP_ERR_NOT_SUPPORTED;
    }
    if (stat(path, &file_stat) != 0) {
        *out_error_message = cap_llm_inspect_format("Image file not found: %s", path);
        return ESP_ERR_NOT_FOUND;
    }
    if (file_stat.st_size <= 0) {
        *out_error_message = cap_llm_inspect_format("Image file is empty: %s", path);
        return ESP_ERR_INVALID_SIZE;
    }
    if ((uint64_t)file_stat.st_size > (uint64_t)image_max_bytes) {
        *out_error_message = cap_llm_inspect_format(
            "Image is too large (%llu bytes > %zu bytes)",
            (unsigned long long)file_stat.st_size,
            image_max_bytes);
        return ESP_ERR_INVALID_SIZE;
    }

    file_size = (size_t)file_stat.st_size;
    if (file_size > (SIZE_MAX - 2) / 4 * 3) {
        *out_error_message = strdup("Image is too large to encode");
        return ESP_ERR_INVALID_SIZE;
    }
    encoded_capacity = ((file_size + 2) / 3) * 4;

    file = fopen(path, "rb");
    if (!file) {
        *out_error_message = cap_llm_inspect_format("Failed to open image: %s", path);
        return ESP_FAIL;
    }

    raw = malloc(file_size);
    encoded = calloc(1, encoded_capacity + 1);
    if (!raw || !encoded) {
        *out_error_message = strdup("Out of memory preparing image");
        err = ESP_ERR_NO_MEM;
        goto cleanup;
    }

    read_size = fread(raw, 1, file_size, file);
    if (read_size != file_size) {
        *out_error_message = cap_llm_inspect_format("Failed to read image: %s", path);
        err = ESP_FAIL;
        goto cleanup;
    }
    if (mbedtls_base64_encode(encoded,
                              encoded_capacity + 1,
                              &encoded_size,
                              raw,
                              file_size) != 0) {
        *out_error_message = strdup("Failed to base64-encode image");
        err = ESP_FAIL;
        goto cleanup;
    }

    encoded[encoded_size] = '\0';
    out_media->base64_data = (char *)encoded;
    out_media->original_size = file_size;
    strlcpy(out_media->mime_type, mime_type, sizeof(out_media->mime_type));
    encoded = NULL;

cleanup:
    if (file) {
        fclose(file);
    }
    free(raw);
    free(encoded);
    return err;
}

void cap_llm_inspect_media_free(cap_llm_inspect_media_t *media)
{
    if (!media) {
        return;
    }

    free(media->base64_data);
    memset(media, 0, sizeof(*media));
}
