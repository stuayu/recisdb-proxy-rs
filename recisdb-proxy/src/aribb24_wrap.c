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

/*
 * Decode text lines without resetting the ARIB code-set state between them.
 * STD-B24 Vol. 1 Part 3 §7.1.2.1/§7.1.2.2 defines C0/C1 controls, including
 * APR/APD (0x0D/0x0A). They are display line-position controls, not text;
 * EPG text exposes them as one newline, with CRLF collapsed here.
 */
size_t C_AribB24DecodeToUtf8Lines(const uint8_t* in, size_t in_len, char* out, size_t out_len)
{
    if (!in || !out || out_len == 0 || in_len == 0) {
        return 0;
    }

    arib_instance_t* inst = arib_instance_new(NULL);
    if (!inst) {
        return 0;
    }

    arib_decoder_t* dec = arib_get_decoder(inst);
    if (!dec) {
        arib_instance_destroy(inst);
        return 0;
    }

    arib_initialize_decoder(dec);
    size_t total = 0;
    size_t start = 0;
    int has_output = 0;

    while (start < in_len && total < out_len) {
        size_t end = start;
        while (end < in_len && in[end] != 0x0D && in[end] != 0x0A) {
            end++;
        }

        if (end > start) {
            if (has_output) {
                if (total == out_len) {
                    break;
                }
                out[total++] = '\n';
            }

            if (total < out_len) {
                size_t written = arib_decode_buffer(
                    dec,
                    (const unsigned char*)in + start,
                    end - start,
                    out + total,
                    out_len - total);
                size_t available = out_len - total;
                if (written > available) {
                    written = available;
                }
                total += written;
                if (written > 0) {
                    has_output = 1;
                }
            }
        }

        start = end;
        while (start < in_len && (in[start] == 0x0D || in[start] == 0x0A)) {
            start++;
        }
    }

    arib_finalize_decoder(dec);
    arib_instance_destroy(inst);
    return total;
}
