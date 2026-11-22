#ifndef SKYBER_A1_CODEC_H
#define SKYBER_A1_CODEC_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#ifdef SKYBER_A1_BUILD_DLL
#define A1_API __declspec(dllexport)
#else
#define A1_API
#endif

typedef struct A1Codec A1Codec;

enum A1Track { A1_MICROPHONE, A1_CAMERA, A1_SCREEN };

typedef struct {
    uint32_t frames;
    uint32_t samples;
    uint32_t crossings;
    uint32_t fingerprint;
} A1Decoded;

A1_API int32_t a1_encoder_open(uint32_t track, uint32_t source, A1Codec **out);
A1_API int32_t a1_decoder_open(uint32_t track, const uint8_t *config, uint32_t size, A1Codec **out);
A1_API int32_t a1_encoder_config(const A1Codec *codec, uint8_t *out, uint32_t capacity);
A1_API int32_t a1_encode(A1Codec *codec, uint8_t *out, uint32_t capacity, uint32_t *key);
/* Inputs are borrowed for the call and copied into encoder-owned frames. */
A1_API int32_t skyber_encode_pcm_f32(A1Codec *codec, const float *samples, uint32_t count,
                                     uint8_t *out, uint32_t capacity, uint32_t *key);
A1_API int32_t skyber_encode_i420(A1Codec *codec, const uint8_t *pixels, uint32_t size,
                                 uint8_t *out, uint32_t capacity, uint32_t *key);
A1_API int32_t a1_decode(A1Codec *codec, const uint8_t *data, uint32_t size, A1Decoded *out);
/* Playback data is stereo S16LE PCM or packed I420, copied into caller memory. */
A1_API int32_t skyber_decode_output(A1Codec *codec, const uint8_t *data, uint32_t size,
                                    A1Decoded *out, uint8_t *frames, uint32_t capacity,
                                    uint32_t *written);
A1_API void a1_codec_close(A1Codec *codec);

/* The MSVC probe consumes this single entry point. Its output belongs to the caller. */
A1_API int32_t skyber_a1_codec_probe_v1(uint32_t *decoded_samples);

#ifdef __cplusplus
}
#endif
#endif
