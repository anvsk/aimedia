/*
 * Copyright 2026 aimedia contributors
 * SPDX-License-Identifier: Apache-2.0
 *
 * Stable, minimal C boundary around the pinned Android libxaac command APIs.
 */

#ifndef AIMEDIA_XAAC_H
#define AIMEDIA_XAAC_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define AIMEDIA_XAAC_OK 0
#define AIMEDIA_XAAC_NEED_MORE_INPUT 1
#define AIMEDIA_XAAC_INVALID_ARGUMENT -1
#define AIMEDIA_XAAC_OUT_OF_MEMORY -2
#define AIMEDIA_XAAC_INITIALIZATION_ERROR -3
#define AIMEDIA_XAAC_UNSUPPORTED_FORMAT -4
#define AIMEDIA_XAAC_CORRUPT_INPUT -5
#define AIMEDIA_XAAC_PROCESSING_ERROR -6
#define AIMEDIA_XAAC_OUTPUT_TOO_SMALL -7
#define AIMEDIA_XAAC_INPUT_LIMIT_EXCEEDED -8

#define AIMEDIA_XAAC_SAMPLES_PER_FRAME 1024
#define AIMEDIA_XAAC_CHANNELS 2
#define AIMEDIA_XAAC_INTERLEAVED_SAMPLES \
  (AIMEDIA_XAAC_SAMPLES_PER_FRAME * AIMEDIA_XAAC_CHANNELS)

typedef struct aimedia_xaac_decoder aimedia_xaac_decoder;
typedef struct aimedia_xaac_encoder aimedia_xaac_encoder;

int32_t aimedia_xaac_decoder_create(aimedia_xaac_decoder **decoder,
                                    int32_t *native_error);

int32_t aimedia_xaac_decoder_decode(aimedia_xaac_decoder *decoder,
                                    const uint8_t *adts,
                                    size_t adts_length,
                                    float *pcm,
                                    size_t pcm_capacity_samples,
                                    size_t *pcm_samples,
                                    int32_t *native_error);

int32_t aimedia_xaac_decoder_flush(aimedia_xaac_decoder *decoder,
                                   float *pcm,
                                   size_t pcm_capacity_samples,
                                   size_t *pcm_samples,
                                   int32_t *native_error);

void aimedia_xaac_decoder_destroy(aimedia_xaac_decoder *decoder);

int32_t aimedia_xaac_encoder_create(aimedia_xaac_encoder **encoder,
                                    int32_t *native_error);

int32_t aimedia_xaac_encoder_encode(aimedia_xaac_encoder *encoder,
                                    const float *pcm,
                                    size_t pcm_samples,
                                    uint8_t *adts,
                                    size_t adts_capacity,
                                    size_t *adts_length,
                                    int32_t *native_error);

void aimedia_xaac_encoder_destroy(aimedia_xaac_encoder *encoder);

#ifdef __cplusplus
}
#endif

#endif
