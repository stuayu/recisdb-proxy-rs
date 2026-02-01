// src/aribb24_wrap.c
#include <stddef.h>
#include <stdint.h>

#include "aribb24/aribb24.h"
#include "aribb24/decoder.h"

size_t C_AribB24DecodeToUtf8(const uint8_t* in, size_t in_len, char* out, size_t out_len)
{
    if (!in || !out || out_len == 0) {
        return 0;
    }

    // arib_instance_t を作って decoder を取得（opaque 型をスタック確保しない）[1](https://github.com/nkoriyama/aribb24/blob/master/src/aribb24/aribb24.h)
    arib_instance_t* inst = arib_instance_new(NULL);
    if (!inst) {
        return 0;
    }

    arib_decoder_t* dec = arib_get_decoder(inst);
    if (!dec) {
        arib_instance_destroy(inst);
        return 0;
    }

    // デコード [5](https://www.windowsmode.com/fix-windows-error-code-0xc0000005/)
    arib_initialize_decoder(dec);
    size_t written = arib_decode_buffer(dec, (const unsigned char*)in, in_len, out, out_len);
    arib_finalize_decoder(dec);

    arib_instance_destroy(inst);
    return written;
}
