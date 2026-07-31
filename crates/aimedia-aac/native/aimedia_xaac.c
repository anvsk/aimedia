/*
 * Copyright 2026 aimedia contributors
 * SPDX-License-Identifier: Apache-2.0
 */

#include "aimedia_xaac.h"

#include <math.h>
#include <stdlib.h>
#include <string.h>

#include "ixheaac_error_standards.h"
#include "ixheaac_type_def.h"
#include "ixheaacd_aac_config.h"
#include "ixheaacd_apicmd_standards.h"
#include "ixheaacd_memory_standards.h"
#include "ixheaace_api.h"

#define AIMEDIA_XAAC_PENDING_CAPACITY (64U * 1024U)
#define AIMEDIA_XAAC_DECODER_ALLOCATION_LIMIT 32U
#define AIMEDIA_XAAC_AAC_LC 2
#define AIMEDIA_XAAC_SAMPLE_RATE 48000
#define AIMEDIA_XAAC_PCM_BITS 16
#define AIMEDIA_XAAC_BITRATE 128000
#define AIMEDIA_XAAC_BIT_RESERVOIR 768

typedef IA_ERRORCODE (*aimedia_xaac_decoder_api)(pVOID, WORD32, WORD32, pVOID);

extern IA_ERRORCODE ixheaacd_dec_api(pVOID p_ia_module_obj,
                                     WORD32 i_cmd,
                                     WORD32 i_idx,
                                     pVOID pv_value);

struct aimedia_xaac_decoder {
  aimedia_xaac_decoder_api api;
  void *api_object;
  void *allocations[AIMEDIA_XAAC_DECODER_ALLOCATION_LIMIT];
  size_t allocation_count;
  uint8_t *input;
  size_t input_capacity;
  int16_t *output;
  size_t output_capacity_bytes;
  uint8_t pending[AIMEDIA_XAAC_PENDING_CAPACITY];
  size_t pending_length;
  int initialized;
  int input_over;
};

struct aimedia_xaac_encoder {
  ixheaace_input_config input_config;
  ixheaace_output_config output_config;
  void *api_object;
  int16_t *input;
  uint8_t *output;
};

static void aimedia_xaac_set_native_error(int32_t *native_error,
                                          IA_ERRORCODE value) {
  if (native_error != NULL) {
    *native_error = (int32_t)value;
  }
}

static int aimedia_xaac_is_fatal(IA_ERRORCODE error) {
  return (((UWORD32)error) & ((UWORD32)IA_FATAL_ERROR)) != 0;
}

static void *aimedia_xaac_aligned_allocate(size_t size, size_t alignment) {
  void *pointer = NULL;
  size_t effective_alignment = alignment;
  if (effective_alignment < sizeof(void *)) {
    effective_alignment = sizeof(void *);
  }
  if (size == 0 || (effective_alignment & (effective_alignment - 1U)) != 0) {
    return NULL;
  }
  if (posix_memalign(&pointer, effective_alignment, size) != 0) {
    return NULL;
  }
  memset(pointer, 0, size);
  return pointer;
}

static void *aimedia_xaac_encoder_allocate(UWORD32 size, UWORD32 alignment) {
  return aimedia_xaac_aligned_allocate((size_t)size, (size_t)alignment);
}

static void aimedia_xaac_encoder_free(pVOID pointer) {
  free(pointer);
}

static int32_t aimedia_xaac_decoder_call(aimedia_xaac_decoder *decoder,
                                         WORD32 command,
                                         WORD32 index,
                                         void *value,
                                         int32_t status,
                                         int32_t *native_error) {
  IA_ERRORCODE error = decoder->api(decoder->api_object, command, index, value);
  aimedia_xaac_set_native_error(native_error, error);
  if (error == IA_NO_ERROR) {
    return AIMEDIA_XAAC_OK;
  }
  return status;
}

static void *aimedia_xaac_decoder_allocate(aimedia_xaac_decoder *decoder,
                                           size_t size,
                                           size_t alignment) {
  void *allocation;
  if (decoder->allocation_count >= AIMEDIA_XAAC_DECODER_ALLOCATION_LIMIT) {
    return NULL;
  }
  allocation = aimedia_xaac_aligned_allocate(size, alignment);
  if (allocation == NULL) {
    return NULL;
  }
  decoder->allocations[decoder->allocation_count++] = allocation;
  return allocation;
}

static void aimedia_xaac_decoder_consume_pending(aimedia_xaac_decoder *decoder,
                                                 size_t consumed) {
  if (consumed >= decoder->pending_length) {
    decoder->pending_length = 0;
    return;
  }
  memmove(decoder->pending, decoder->pending + consumed,
          decoder->pending_length - consumed);
  decoder->pending_length -= consumed;
}

static int32_t aimedia_xaac_decoder_append(aimedia_xaac_decoder *decoder,
                                          const uint8_t *input,
                                          size_t input_length) {
  if (input_length == 0) {
    return AIMEDIA_XAAC_OK;
  }
  if (input == NULL) {
    return AIMEDIA_XAAC_INVALID_ARGUMENT;
  }
  if (input_length > AIMEDIA_XAAC_PENDING_CAPACITY - decoder->pending_length) {
    return AIMEDIA_XAAC_INPUT_LIMIT_EXCEEDED;
  }
  memcpy(decoder->pending + decoder->pending_length, input, input_length);
  decoder->pending_length += input_length;
  return AIMEDIA_XAAC_OK;
}

static size_t aimedia_xaac_decoder_stage_input(
    aimedia_xaac_decoder *decoder) {
  size_t staged = decoder->pending_length;
  if (staged > decoder->input_capacity) {
    staged = decoder->input_capacity;
  }
  if (staged != 0) {
    memcpy(decoder->input, decoder->pending, staged);
  }
  return staged;
}

static int32_t aimedia_xaac_decoder_validate_format(
    aimedia_xaac_decoder *decoder,
    int32_t *native_error) {
  WORD32 sample_rate = 0;
  WORD32 channels = 0;
  WORD32 pcm_bits = 0;
  WORD32 audio_object_type = 0;
  int32_t status;

  status = aimedia_xaac_decoder_call(
      decoder, IA_API_CMD_GET_CONFIG_PARAM,
      IA_XHEAAC_DEC_CONFIG_PARAM_SAMP_FREQ, &sample_rate,
      AIMEDIA_XAAC_INITIALIZATION_ERROR, native_error);
  if (status != AIMEDIA_XAAC_OK) {
    return status;
  }
  status = aimedia_xaac_decoder_call(
      decoder, IA_API_CMD_GET_CONFIG_PARAM,
      IA_XHEAAC_DEC_CONFIG_PARAM_NUM_CHANNELS, &channels,
      AIMEDIA_XAAC_INITIALIZATION_ERROR, native_error);
  if (status != AIMEDIA_XAAC_OK) {
    return status;
  }
  status = aimedia_xaac_decoder_call(
      decoder, IA_API_CMD_GET_CONFIG_PARAM,
      IA_XHEAAC_DEC_CONFIG_PARAM_PCM_WDSZ, &pcm_bits,
      AIMEDIA_XAAC_INITIALIZATION_ERROR, native_error);
  if (status != AIMEDIA_XAAC_OK) {
    return status;
  }
  status = aimedia_xaac_decoder_call(
      decoder, IA_API_CMD_GET_CONFIG_PARAM, IA_XHEAAC_DEC_CONFIG_PARAM_AOT,
      &audio_object_type, AIMEDIA_XAAC_INITIALIZATION_ERROR, native_error);
  if (status != AIMEDIA_XAAC_OK) {
    return status;
  }

  if (sample_rate != AIMEDIA_XAAC_SAMPLE_RATE ||
      channels != AIMEDIA_XAAC_CHANNELS ||
      pcm_bits != AIMEDIA_XAAC_PCM_BITS ||
      audio_object_type != AIMEDIA_XAAC_AAC_LC) {
    return AIMEDIA_XAAC_UNSUPPORTED_FORMAT;
  }
  return AIMEDIA_XAAC_OK;
}

static int32_t aimedia_xaac_decoder_initialize(
    aimedia_xaac_decoder *decoder,
    int32_t *native_error) {
  while (!decoder->initialized && decoder->pending_length != 0) {
    size_t staged = aimedia_xaac_decoder_stage_input(decoder);
    WORD32 input_bytes = (WORD32)staged;
    WORD32 init_done = 0;
    WORD32 consumed = 0;
    IA_ERRORCODE error;
    int32_t status;

    status = aimedia_xaac_decoder_call(
        decoder, IA_API_CMD_SET_INPUT_BYTES, 0, &input_bytes,
        AIMEDIA_XAAC_INITIALIZATION_ERROR, native_error);
    if (status != AIMEDIA_XAAC_OK) {
      return status;
    }

    error = decoder->api(decoder->api_object, IA_API_CMD_INIT,
                         IA_CMD_TYPE_INIT_PROCESS, NULL);
    aimedia_xaac_set_native_error(native_error, error);
    if (aimedia_xaac_is_fatal(error)) {
      return AIMEDIA_XAAC_CORRUPT_INPUT;
    }

    status = aimedia_xaac_decoder_call(
        decoder, IA_API_CMD_INIT, IA_CMD_TYPE_INIT_DONE_QUERY, &init_done,
        AIMEDIA_XAAC_INITIALIZATION_ERROR, native_error);
    if (status != AIMEDIA_XAAC_OK) {
      return status;
    }
    status = aimedia_xaac_decoder_call(
        decoder, IA_API_CMD_GET_CURIDX_INPUT_BUF, 0, &consumed,
        AIMEDIA_XAAC_INITIALIZATION_ERROR, native_error);
    if (status != AIMEDIA_XAAC_OK || consumed < 0 ||
        (size_t)consumed > staged) {
      return AIMEDIA_XAAC_INITIALIZATION_ERROR;
    }
    aimedia_xaac_decoder_consume_pending(decoder, (size_t)consumed);

    if (init_done != 0) {
      decoder->initialized = 1;
      return aimedia_xaac_decoder_validate_format(decoder, native_error);
    }
    if (consumed == 0) {
      break;
    }
  }
  return decoder->initialized ? AIMEDIA_XAAC_OK
                              : AIMEDIA_XAAC_NEED_MORE_INPUT;
}

static int32_t aimedia_xaac_decoder_execute(
    aimedia_xaac_decoder *decoder,
    float *pcm,
    size_t pcm_capacity_samples,
    size_t *pcm_samples,
    int32_t *native_error) {
  size_t staged = aimedia_xaac_decoder_stage_input(decoder);
  WORD32 input_bytes = (WORD32)staged;
  WORD32 consumed = 0;
  WORD32 output_bytes = 0;
  IA_ERRORCODE error;
  size_t sample_count;
  size_t index;
  int32_t status;

  if (staged == 0 && !decoder->input_over) {
    return AIMEDIA_XAAC_NEED_MORE_INPUT;
  }
  status = aimedia_xaac_decoder_call(
      decoder, IA_API_CMD_SET_INPUT_BYTES, 0, &input_bytes,
      AIMEDIA_XAAC_PROCESSING_ERROR, native_error);
  if (status != AIMEDIA_XAAC_OK) {
    return status;
  }

  error = decoder->api(decoder->api_object, IA_API_CMD_EXECUTE,
                       IA_CMD_TYPE_DO_EXECUTE, NULL);
  aimedia_xaac_set_native_error(native_error, error);
  if (aimedia_xaac_is_fatal(error)) {
    return AIMEDIA_XAAC_CORRUPT_INPUT;
  }

  status = aimedia_xaac_decoder_call(
      decoder, IA_API_CMD_GET_CURIDX_INPUT_BUF, 0, &consumed,
      AIMEDIA_XAAC_PROCESSING_ERROR, native_error);
  if (status != AIMEDIA_XAAC_OK || consumed < 0 ||
      (size_t)consumed > staged) {
    return AIMEDIA_XAAC_PROCESSING_ERROR;
  }
  aimedia_xaac_decoder_consume_pending(decoder, (size_t)consumed);

  status = aimedia_xaac_decoder_call(
      decoder, IA_API_CMD_GET_OUTPUT_BYTES, 0, &output_bytes,
      AIMEDIA_XAAC_PROCESSING_ERROR, native_error);
  if (status != AIMEDIA_XAAC_OK || output_bytes < 0 ||
      (size_t)output_bytes > decoder->output_capacity_bytes ||
      ((size_t)output_bytes % sizeof(int16_t)) != 0) {
    return AIMEDIA_XAAC_PROCESSING_ERROR;
  }

  sample_count = (size_t)output_bytes / sizeof(int16_t);
  if (sample_count > pcm_capacity_samples) {
    return AIMEDIA_XAAC_OUTPUT_TOO_SMALL;
  }
  for (index = 0; index < sample_count; ++index) {
    pcm[index] = (float)decoder->output[index] / 32768.0f;
  }
  *pcm_samples = sample_count;

  if (error != IA_NO_ERROR) {
    return AIMEDIA_XAAC_CORRUPT_INPUT;
  }
  return sample_count == 0 ? AIMEDIA_XAAC_NEED_MORE_INPUT : AIMEDIA_XAAC_OK;
}

int32_t aimedia_xaac_decoder_create(aimedia_xaac_decoder **decoder_out,
                                    int32_t *native_error) {
  aimedia_xaac_decoder *decoder;
  UWORD32 api_size = 0;
  UWORD32 memtabs_size = 0;
  UWORD32 memory_count = 0;
  UWORD32 index;
  UWORD32 pcm_bits = AIMEDIA_XAAC_PCM_BITS;
  IA_ERRORCODE error;

  if (decoder_out == NULL) {
    return AIMEDIA_XAAC_INVALID_ARGUMENT;
  }
  *decoder_out = NULL;
  aimedia_xaac_set_native_error(native_error, IA_NO_ERROR);

  decoder = (aimedia_xaac_decoder *)calloc(1, sizeof(*decoder));
  if (decoder == NULL) {
    return AIMEDIA_XAAC_OUT_OF_MEMORY;
  }
  decoder->api = ixheaacd_dec_api;

  error = decoder->api(NULL, IA_API_CMD_GET_API_SIZE, 0, &api_size);
  aimedia_xaac_set_native_error(native_error, error);
  if (error != IA_NO_ERROR || api_size == 0) {
    aimedia_xaac_decoder_destroy(decoder);
    return AIMEDIA_XAAC_INITIALIZATION_ERROR;
  }
  decoder->api_object =
      aimedia_xaac_decoder_allocate(decoder, (size_t)api_size, 8U);
  if (decoder->api_object == NULL) {
    aimedia_xaac_decoder_destroy(decoder);
    return AIMEDIA_XAAC_OUT_OF_MEMORY;
  }

  error = decoder->api(decoder->api_object, IA_API_CMD_INIT,
                       IA_CMD_TYPE_INIT_API_PRE_CONFIG_PARAMS, NULL);
  aimedia_xaac_set_native_error(native_error, error);
  if (error != IA_NO_ERROR) {
    aimedia_xaac_decoder_destroy(decoder);
    return AIMEDIA_XAAC_INITIALIZATION_ERROR;
  }
  error = decoder->api(decoder->api_object, IA_API_CMD_SET_CONFIG_PARAM,
                       IA_XHEAAC_DEC_CONFIG_PARAM_PCM_WDSZ, &pcm_bits);
  aimedia_xaac_set_native_error(native_error, error);
  if (error != IA_NO_ERROR) {
    aimedia_xaac_decoder_destroy(decoder);
    return AIMEDIA_XAAC_INITIALIZATION_ERROR;
  }

  error = decoder->api(decoder->api_object, IA_API_CMD_GET_MEMTABS_SIZE, 0,
                       &memtabs_size);
  aimedia_xaac_set_native_error(native_error, error);
  if (error != IA_NO_ERROR || memtabs_size == 0) {
    aimedia_xaac_decoder_destroy(decoder);
    return AIMEDIA_XAAC_INITIALIZATION_ERROR;
  }
  {
    void *memtabs =
        aimedia_xaac_decoder_allocate(decoder, (size_t)memtabs_size, 8U);
    if (memtabs == NULL) {
      aimedia_xaac_decoder_destroy(decoder);
      return AIMEDIA_XAAC_OUT_OF_MEMORY;
    }
    error = decoder->api(decoder->api_object, IA_API_CMD_SET_MEMTABS_PTR, 0,
                         memtabs);
  }
  aimedia_xaac_set_native_error(native_error, error);
  if (error != IA_NO_ERROR) {
    aimedia_xaac_decoder_destroy(decoder);
    return AIMEDIA_XAAC_INITIALIZATION_ERROR;
  }

  error = decoder->api(decoder->api_object, IA_API_CMD_INIT,
                       IA_CMD_TYPE_INIT_API_POST_CONFIG_PARAMS, NULL);
  aimedia_xaac_set_native_error(native_error, error);
  if (error != IA_NO_ERROR) {
    aimedia_xaac_decoder_destroy(decoder);
    return AIMEDIA_XAAC_INITIALIZATION_ERROR;
  }
  error = decoder->api(decoder->api_object, IA_API_CMD_GET_N_MEMTABS, 0,
                       &memory_count);
  aimedia_xaac_set_native_error(native_error, error);
  if (error != IA_NO_ERROR ||
      memory_count > AIMEDIA_XAAC_DECODER_ALLOCATION_LIMIT - 2U) {
    aimedia_xaac_decoder_destroy(decoder);
    return AIMEDIA_XAAC_INITIALIZATION_ERROR;
  }

  for (index = 0; index < memory_count; ++index) {
    UWORD32 size = 0;
    UWORD32 alignment = 0;
    UWORD32 type = 0;
    void *memory;

    if (decoder->api(decoder->api_object, IA_API_CMD_GET_MEM_INFO_SIZE,
                     (WORD32)index, &size) != IA_NO_ERROR ||
        decoder->api(decoder->api_object, IA_API_CMD_GET_MEM_INFO_ALIGNMENT,
                     (WORD32)index, &alignment) != IA_NO_ERROR ||
        decoder->api(decoder->api_object, IA_API_CMD_GET_MEM_INFO_TYPE,
                     (WORD32)index, &type) != IA_NO_ERROR ||
        size == 0 || alignment == 0) {
      aimedia_xaac_decoder_destroy(decoder);
      return AIMEDIA_XAAC_INITIALIZATION_ERROR;
    }
    memory =
        aimedia_xaac_decoder_allocate(decoder, (size_t)size, (size_t)alignment);
    if (memory == NULL) {
      aimedia_xaac_decoder_destroy(decoder);
      return AIMEDIA_XAAC_OUT_OF_MEMORY;
    }
    error = decoder->api(decoder->api_object, IA_API_CMD_SET_MEM_PTR,
                         (WORD32)index, memory);
    aimedia_xaac_set_native_error(native_error, error);
    if (error != IA_NO_ERROR) {
      aimedia_xaac_decoder_destroy(decoder);
      return AIMEDIA_XAAC_INITIALIZATION_ERROR;
    }
    if (type == IA_MEMTYPE_INPUT) {
      decoder->input = (uint8_t *)memory;
      decoder->input_capacity = (size_t)size;
    } else if (type == IA_MEMTYPE_OUTPUT) {
      decoder->output = (int16_t *)memory;
      decoder->output_capacity_bytes = (size_t)size;
    }
  }

  if (decoder->input == NULL || decoder->output == NULL) {
    aimedia_xaac_decoder_destroy(decoder);
    return AIMEDIA_XAAC_INITIALIZATION_ERROR;
  }
  *decoder_out = decoder;
  return AIMEDIA_XAAC_OK;
}

int32_t aimedia_xaac_decoder_decode(aimedia_xaac_decoder *decoder,
                                    const uint8_t *adts,
                                    size_t adts_length,
                                    float *pcm,
                                    size_t pcm_capacity_samples,
                                    size_t *pcm_samples,
                                    int32_t *native_error) {
  int32_t status;
  if (decoder == NULL || pcm == NULL || pcm_samples == NULL ||
      pcm_capacity_samples < AIMEDIA_XAAC_INTERLEAVED_SAMPLES ||
      decoder->input_over) {
    return AIMEDIA_XAAC_INVALID_ARGUMENT;
  }
  *pcm_samples = 0;
  aimedia_xaac_set_native_error(native_error, IA_NO_ERROR);

  status = aimedia_xaac_decoder_append(decoder, adts, adts_length);
  if (status != AIMEDIA_XAAC_OK) {
    return status;
  }
  status = aimedia_xaac_decoder_initialize(decoder, native_error);
  if (status != AIMEDIA_XAAC_OK) {
    return status;
  }
  return aimedia_xaac_decoder_execute(decoder, pcm, pcm_capacity_samples,
                                      pcm_samples, native_error);
}

int32_t aimedia_xaac_decoder_flush(aimedia_xaac_decoder *decoder,
                                   float *pcm,
                                   size_t pcm_capacity_samples,
                                   size_t *pcm_samples,
                                   int32_t *native_error) {
  IA_ERRORCODE error;
  if (decoder == NULL || pcm == NULL || pcm_samples == NULL ||
      pcm_capacity_samples < AIMEDIA_XAAC_INTERLEAVED_SAMPLES) {
    return AIMEDIA_XAAC_INVALID_ARGUMENT;
  }
  *pcm_samples = 0;
  if (!decoder->initialized) {
    return AIMEDIA_XAAC_NEED_MORE_INPUT;
  }
  if (!decoder->input_over) {
    error = decoder->api(decoder->api_object, IA_API_CMD_INPUT_OVER, 0, NULL);
    aimedia_xaac_set_native_error(native_error, error);
    if (error != IA_NO_ERROR) {
      return AIMEDIA_XAAC_PROCESSING_ERROR;
    }
    decoder->input_over = 1;
  }
  return aimedia_xaac_decoder_execute(decoder, pcm, pcm_capacity_samples,
                                      pcm_samples, native_error);
}

void aimedia_xaac_decoder_destroy(aimedia_xaac_decoder *decoder) {
  size_t index;
  if (decoder == NULL) {
    return;
  }
  for (index = decoder->allocation_count; index > 0; --index) {
    free(decoder->allocations[index - 1U]);
  }
  free(decoder);
}

int32_t aimedia_xaac_encoder_create(aimedia_xaac_encoder **encoder_out,
                                    int32_t *native_error) {
  aimedia_xaac_encoder *encoder;
  IA_ERRORCODE error;

  if (encoder_out == NULL) {
    return AIMEDIA_XAAC_INVALID_ARGUMENT;
  }
  *encoder_out = NULL;
  aimedia_xaac_set_native_error(native_error, IA_NO_ERROR);

  encoder = (aimedia_xaac_encoder *)calloc(1, sizeof(*encoder));
  if (encoder == NULL) {
    return AIMEDIA_XAAC_OUT_OF_MEMORY;
  }

  encoder->input_config.ui_pcm_wd_sz = AIMEDIA_XAAC_PCM_BITS;
  encoder->input_config.i_bitrate = AIMEDIA_XAAC_BITRATE;
  encoder->input_config.frame_length = AIMEDIA_XAAC_SAMPLES_PER_FRAME;
  encoder->input_config.frame_cmd_flag = 1;
  encoder->input_config.out_bytes_flag = 1;
  encoder->input_config.user_tns_flag = 1;
  encoder->input_config.aot = AOT_AAC_LC;
  encoder->input_config.i_mps_tree_config = -1;
  encoder->input_config.i_channels = AIMEDIA_XAAC_CHANNELS;
  encoder->input_config.i_samp_freq = AIMEDIA_XAAC_SAMPLE_RATE;
  encoder->input_config.i_use_adts = 1;
  encoder->input_config.i_use_es = 0;
  encoder->input_config.ccfl_idx = 1;
  encoder->input_config.aac_config.sample_rate = AIMEDIA_XAAC_SAMPLE_RATE;
  encoder->input_config.aac_config.bitrate = AIMEDIA_XAAC_BITRATE;
  encoder->input_config.aac_config.num_channels_in = AIMEDIA_XAAC_CHANNELS;
  encoder->input_config.aac_config.num_channels_out = AIMEDIA_XAAC_CHANNELS;
  encoder->input_config.aac_config.inv_quant = 2;
  encoder->input_config.aac_config.use_tns = 1;
  encoder->input_config.aac_config.use_adts = 1;
  encoder->input_config.aac_config.bitreservoir_size =
      AIMEDIA_XAAC_BIT_RESERVOIR;
  encoder->output_config.malloc_xheaace = aimedia_xaac_encoder_allocate;
  encoder->output_config.free_xheaace = aimedia_xaac_encoder_free;

  error = ixheaace_create(&encoder->input_config, &encoder->output_config);
  aimedia_xaac_set_native_error(native_error, error);
  if (error != IA_NO_ERROR) {
    free(encoder);
    return AIMEDIA_XAAC_INITIALIZATION_ERROR;
  }
  encoder->api_object = encoder->output_config.pv_ia_process_api_obj;
  encoder->input = (int16_t *)
      encoder->output_config.mem_info_table[IA_MEMTYPE_INPUT].mem_ptr;
  encoder->output = (uint8_t *)
      encoder->output_config.mem_info_table[IA_MEMTYPE_OUTPUT].mem_ptr;
  if (encoder->api_object == NULL || encoder->input == NULL ||
      encoder->output == NULL ||
      encoder->output_config.input_size !=
          AIMEDIA_XAAC_INTERLEAVED_SAMPLES * (int32_t)sizeof(int16_t)) {
    ixheaace_delete(&encoder->output_config);
    free(encoder);
    return AIMEDIA_XAAC_INITIALIZATION_ERROR;
  }
  *encoder_out = encoder;
  return AIMEDIA_XAAC_OK;
}

int32_t aimedia_xaac_encoder_encode(aimedia_xaac_encoder *encoder,
                                    const float *pcm,
                                    size_t pcm_samples,
                                    uint8_t *adts,
                                    size_t adts_capacity,
                                    size_t *adts_length,
                                    int32_t *native_error) {
  size_t index;
  IA_ERRORCODE error;
  if (encoder == NULL || pcm == NULL || adts == NULL ||
      adts_length == NULL ||
      pcm_samples != AIMEDIA_XAAC_INTERLEAVED_SAMPLES) {
    return AIMEDIA_XAAC_INVALID_ARGUMENT;
  }
  *adts_length = 0;
  for (index = 0; index < pcm_samples; ++index) {
    float value = pcm[index];
    if (!isfinite(value)) {
      value = 0.0f;
    }
    if (value > 1.0f) {
      value = 1.0f;
    } else if (value < -1.0f) {
      value = -1.0f;
    }
    encoder->input[index] =
        value <= -1.0f ? INT16_MIN : (int16_t)lrintf(value * 32767.0f);
  }

  error = ixheaace_process(encoder->api_object, &encoder->input_config,
                           &encoder->output_config);
  aimedia_xaac_set_native_error(native_error, error);
  if (error != IA_NO_ERROR) {
    return aimedia_xaac_is_fatal(error) ? AIMEDIA_XAAC_PROCESSING_ERROR
                                        : AIMEDIA_XAAC_CORRUPT_INPUT;
  }
  if (encoder->output_config.i_out_bytes < 0) {
    return AIMEDIA_XAAC_PROCESSING_ERROR;
  }
  if ((size_t)encoder->output_config.i_out_bytes > adts_capacity) {
    return AIMEDIA_XAAC_OUTPUT_TOO_SMALL;
  }
  memcpy(adts, encoder->output,
         (size_t)encoder->output_config.i_out_bytes);
  *adts_length = (size_t)encoder->output_config.i_out_bytes;
  return AIMEDIA_XAAC_OK;
}

void aimedia_xaac_encoder_destroy(aimedia_xaac_encoder *encoder) {
  if (encoder == NULL) {
    return;
  }
  if (encoder->api_object != NULL) {
    ixheaace_delete(&encoder->output_config);
  }
  free(encoder);
}
