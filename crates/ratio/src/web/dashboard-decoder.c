#include <stdint.h>
#define ZSTD_STATIC_LINKING_ONLY
#include "zstd.h"

#define CHUNK (128 * 1024)
static uint8_t input[CHUNK + 1024];
static uint8_t output[CHUNK];
static ZSTD_DCtx *decoder;
static ZSTD_DDict *dictionary;

uint8_t *ratio_input(void) { return input; }
uint8_t *ratio_output(void) { return output; }

int ratio_set_dictionary(unsigned size, unsigned id) {
    if (size > 64 * 1024 || id == 0 || ZSTD_getDictID_fromDict(output, size) != id)
        return 0;
    ZSTD_DDict *prepared = ZSTD_createDDict(output, size);
    if (!prepared)
        return 0;
    ZSTD_freeDDict(dictionary);
    dictionary = prepared;
    return 1;
}

int ratio_decode(unsigned size, unsigned decoded, unsigned id) {
    if (size > sizeof(input) || decoded == 0 || decoded > sizeof(output))
        return 0;
    ZSTD_frameHeader header;
    if (ZSTD_getFrameHeader(&header, input, size) != 0 ||
        header.frameType != ZSTD_frame || header.frameContentSize != decoded ||
        header.windowSize > CHUNK || header.dictID != id ||
        ZSTD_findFrameCompressedSize(input, size) != size)
        return 0;
    if (!decoder)
        decoder = ZSTD_createDCtx();
    if (!decoder || (id && (!dictionary || ZSTD_getDictID_fromDDict(dictionary) != id)))
        return 0;
    size_t result = id
        ? ZSTD_decompress_usingDDict(decoder, output, decoded, input, size, dictionary)
        : ZSTD_decompressDCtx(decoder, output, decoded, input, size);
    return result == decoded;
}
