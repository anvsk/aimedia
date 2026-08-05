#if defined(AIMEDIA_FFNV_CODEC_HEADERS)
#include <ffnvcodec/dynlink_cuda.h>
#include <ffnvcodec/dynlink_cuviddec.h>
#include <ffnvcodec/dynlink_nvcuvid.h>
#include <ffnvcodec/nvEncodeAPI.h>
#else
#include <cuda.h>
#include <cuviddec.h>
#include <nvcuvid.h>
#include <nvEncodeAPI.h>
#endif
