/* Thin C ABI over minimp3's pull API, for Rust FFI (no bindgen needed). */
#include <stdlib.h>
#define MINIMP3_IMPLEMENTATION
#include "minimp3.h"

mp3dec_t *sr_mp3_new(void) {
    mp3dec_t *d = (mp3dec_t *)malloc(sizeof(mp3dec_t));
    if (d) {
        mp3dec_init(d);
    }
    return d;
}

void sr_mp3_free(mp3dec_t *d) { free(d); }

/* Decode one frame. Returns samples-per-channel; *frame_bytes advances the
   caller's cursor (0 means "need more data" / end). */
int sr_mp3_decode(mp3dec_t *d, const unsigned char *buf, int len, short *pcm,
                  int *channels, int *hz, int *frame_bytes) {
    mp3dec_frame_info_t info;
    int samples = mp3dec_decode_frame(d, buf, len, pcm, &info);
    *channels = info.channels;
    *hz = info.hz;
    *frame_bytes = info.frame_bytes;
    return samples;
}
