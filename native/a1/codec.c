#include "codec.h"

#include <errno.h>
#include <limits.h>
#include <math.h>
#include <stdbool.h>
#include <string.h>

#include <libavcodec/avcodec.h>
#include <libavutil/mem.h>
#include <libavutil/opt.h>
#include <libavutil/samplefmt.h>

enum { RATE = 48000, AUDIO_SAMPLES = 960, FPS = 15, WIDTH = 320, HEIGHT = 180 };

struct A1Codec {
    AVCodecContext *ctx;
    AVFrame *frame;
    AVPacket *packet;
    uint32_t track;
    uint32_t source;
    int64_t number;
    bool previous_positive;
};

void a1_codec_close(A1Codec *codec) {
    if (codec) {
        avcodec_free_context(&codec->ctx);
        av_frame_free(&codec->frame);
        av_packet_free(&codec->packet);
        av_free(codec);
    }
}

static int open_codec(uint32_t track, uint32_t source, bool encoder,
                      const uint8_t *config, uint32_t size, A1Codec **out) {
    if (!out || track > A1_SCREEN || size > 65536 || (size && !config)) {
        return AVERROR(EINVAL);
    }
    *out = NULL;
    const bool audio = track == A1_MICROPHONE;
    const AVCodec *implementation;
    if (encoder) {
        implementation = avcodec_find_encoder_by_name(audio ? "libopus" : "libx264");
    } else {
        implementation = avcodec_find_decoder_by_name(audio ? "libopus" : "h264");
    }
    if (!implementation) {
        return AVERROR(ENOSYS);
    }
    A1Codec *codec = av_mallocz(sizeof(*codec));
    if (!codec) {
        return AVERROR(ENOMEM);
    }
    codec->ctx = avcodec_alloc_context3(implementation);
    codec->frame = av_frame_alloc();
    codec->packet = av_packet_alloc();
    if (!codec->ctx || !codec->frame || !codec->packet) {
        a1_codec_close(codec);
        return AVERROR(ENOMEM);
    }
    codec->track = track;
    codec->source = source;
    AVCodecContext *ctx = codec->ctx;
    ctx->thread_count = 1;
    ctx->flags = AV_CODEC_FLAG_LOW_DELAY;
    int result = 0;
    if (audio) {
        ctx->sample_rate = RATE;
        ctx->ch_layout = (AVChannelLayout)AV_CHANNEL_LAYOUT_STEREO;
        ctx->time_base = (AVRational){1, RATE};
        ctx->sample_fmt = AV_SAMPLE_FMT_FLT;
        ctx->request_sample_fmt = AV_SAMPLE_FMT_FLT;
        if (encoder) {
            ctx->bit_rate = 128000;
            ctx->flags |= AV_CODEC_FLAG_GLOBAL_HEADER;
            result = av_opt_set(ctx->priv_data, "application", "audio", 0);
            if (result >= 0) {
                result = av_opt_set(ctx->priv_data, "frame_duration", "20", 0);
            }
        }
    } else if (encoder) {
        ctx->width = WIDTH;
        ctx->height = HEIGHT;
        ctx->pix_fmt = AV_PIX_FMT_YUV420P;
        ctx->time_base = (AVRational){1, FPS};
        ctx->framerate = (AVRational){FPS, 1};
        ctx->gop_size = FPS;
        ctx->max_b_frames = 0;
        result = av_opt_set(ctx->priv_data, "preset", "ultrafast", 0);
        if (result >= 0) {
            result = av_opt_set(ctx->priv_data, "tune", "zerolatency", 0);
        }
    }
    if (result < 0) {
        a1_codec_close(codec);
        return result;
    }
    if (size) {
        ctx->extradata = av_mallocz((size_t)size + AV_INPUT_BUFFER_PADDING_SIZE);
        if (!ctx->extradata) {
            a1_codec_close(codec);
            return AVERROR(ENOMEM);
        }
        memcpy(ctx->extradata, config, size);
        ctx->extradata_size = (int)size;
    }
    result = avcodec_open2(ctx, implementation, NULL);
    if (result >= 0 && encoder) {
        AVFrame *frame = codec->frame;
        if (audio) {
            frame->format = ctx->sample_fmt;
            frame->sample_rate = RATE;
            frame->nb_samples = AUDIO_SAMPLES;
            result = av_channel_layout_copy(&frame->ch_layout, &ctx->ch_layout);
        } else {
            frame->format = ctx->pix_fmt;
            frame->width = WIDTH;
            frame->height = HEIGHT;
        }
        if (result >= 0) {
            result = av_frame_get_buffer(frame, 0);
        }
    }
    if (result < 0) {
        a1_codec_close(codec);
        return result;
    }
    *out = codec;
    return 0;
}

int32_t a1_encoder_open(uint32_t track, uint32_t source, A1Codec **out) {
    return open_codec(track, source, true, NULL, 0, out);
}

int32_t a1_decoder_open(uint32_t track, const uint8_t *config, uint32_t size, A1Codec **out) {
    return open_codec(track, 0, false, config, size, out);
}

int32_t a1_encoder_config(const A1Codec *codec, uint8_t *out, uint32_t capacity) {
    const int size = codec->ctx->extradata_size;
    if (size < 0 || (uint32_t)size > capacity) {
        return AVERROR(ENOSPC);
    }
    if (size) {
        memcpy(out, codec->ctx->extradata, (size_t)size);
    }
    return size;
}

static void generate_audio(A1Codec *codec) {
    const double frequency = codec->source == 0 ? 440.0 : 660.0;
    float *samples = (float *)codec->frame->data[0];
    for (int index = 0; index < AUDIO_SAMPLES; ++index) {
        const double phase = (double)(codec->number * AUDIO_SAMPLES + index) / RATE;
        const float sample = (float)(0.3 * sin(6.283185307179586 * frequency * phase));
        samples[index * 2] = sample;
        samples[index * 2 + 1] = sample;
    }
}

static void generate_video(A1Codec *codec) {
    AVFrame *frame = codec->frame;
    const uint32_t phase = (uint32_t)codec->number;
    for (uint32_t y = 0; y < HEIGHT; ++y) {
        uint8_t *row = frame->data[0] + y * frame->linesize[0];
        for (uint32_t x = 0; x < WIDTH; ++x) {
            const uint32_t pattern = codec->track == A1_CAMERA
                ? (x + y + phase * 4) % 180
                : ((x / 16 + y / 12 + phase / 3) % 2) * 140;
            row[x] = (uint8_t)(16 + pattern + codec->source * 18);
        }
    }
    for (int y = 0; y < HEIGHT / 2; ++y) {
        memset(frame->data[1] + y * frame->linesize[1], 100, WIDTH / 2);
        memset(frame->data[2] + y * frame->linesize[2], 150, WIDTH / 2);
    }
}

int32_t a1_encode(A1Codec *codec, uint8_t *out, uint32_t capacity, uint32_t *key) {
    int result = av_frame_make_writable(codec->frame);
    if (result < 0) {
        return result;
    }
    if (codec->track == A1_MICROPHONE) {
        generate_audio(codec);
        codec->frame->pts = codec->number * AUDIO_SAMPLES;
    } else {
        generate_video(codec);
        codec->frame->pts = codec->number;
    }
    ++codec->number;
    result = avcodec_send_frame(codec->ctx, codec->frame);
    if (result < 0) {
        return result;
    }
    result = avcodec_receive_packet(codec->ctx, codec->packet);
    if (result == AVERROR(EAGAIN)) {
        return 0;
    }
    if (result < 0) {
        return result;
    }
    const int size = codec->packet->size;
    if (size < 0 || (uint32_t)size > capacity) {
        av_packet_unref(codec->packet);
        return AVERROR(ENOSPC);
    }
    memcpy(out, codec->packet->data, (size_t)size);
    *key = (codec->packet->flags & AV_PKT_FLAG_KEY) != 0;
    av_packet_unref(codec->packet);
    return size;
}

static int read_audio(A1Codec *codec, A1Decoded *out) {
    const AVFrame *frame = codec->frame;
    if (!frame->extended_data || frame->sample_rate != RATE || frame->ch_layout.nb_channels != 2) {
        return AVERROR_INVALIDDATA;
    }
    const bool planar = av_sample_fmt_is_planar(frame->format) != 0;
    const enum AVSampleFormat format = av_get_packed_sample_fmt(frame->format);
    if (format != AV_SAMPLE_FMT_FLT && format != AV_SAMPLE_FMT_S16) {
        return AVERROR(ENOSYS);
    }
    for (int index = 0; index < frame->nb_samples; ++index) {
        const int offset = planar ? index : index * 2;
        const bool positive = format == AV_SAMPLE_FMT_FLT
            ? ((const float *)frame->extended_data[0])[offset] > 0.0f
            : ((const int16_t *)frame->extended_data[0])[offset] > 0;
        out->crossings += positive && !codec->previous_positive;
        codec->previous_positive = positive;
    }
    out->samples += (uint32_t)frame->nb_samples;
    return 0;
}

static int read_video(const AVFrame *frame, A1Decoded *out) {
    if (frame->width != WIDTH || frame->height != HEIGHT || frame->format != AV_PIX_FMT_YUV420P) {
        return AVERROR_INVALIDDATA;
    }
    uint32_t hash = 2166136261u;
    for (int y = 0; y < HEIGHT; y += 4) {
        for (int x = 0; x < WIDTH; x += 4) {
            hash = (hash ^ frame->data[0][y * frame->linesize[0] + x]) * 16777619u;
        }
    }
    out->fingerprint = hash;
    return 0;
}

int32_t a1_decode(A1Codec *codec, const uint8_t *data, uint32_t size, A1Decoded *out) {
    if (!size || size > INT_MAX) {
        return AVERROR(EINVAL);
    }
    *out = (A1Decoded){0};
    int result = av_new_packet(codec->packet, (int)size);
    if (result < 0) {
        return result;
    }
    memcpy(codec->packet->data, data, size);
    result = avcodec_send_packet(codec->ctx, codec->packet);
    av_packet_unref(codec->packet);
    if (result < 0) {
        return result;
    }
    while ((result = avcodec_receive_frame(codec->ctx, codec->frame)) >= 0) {
        ++out->frames;
        result = codec->track == A1_MICROPHONE
            ? read_audio(codec, out)
            : read_video(codec->frame, out);
        av_frame_unref(codec->frame);
        if (result < 0) {
            return result;
        }
    }
    return result == AVERROR(EAGAIN) ? 0 : result;
}

int32_t skyber_a1_codec_probe_v1(uint32_t *decoded_samples) {
    if (!decoded_samples) {
        return AVERROR(EINVAL);
    }
    *decoded_samples = 0;
    A1Codec *encoder = NULL;
    A1Codec *decoder = NULL;
    uint8_t config[256];
    uint8_t packet[4096];
    uint32_t key;
    int result = a1_encoder_open(A1_MICROPHONE, 0, &encoder);
    if (result >= 0) {
        result = a1_encoder_config(encoder, config, sizeof(config));
    }
    if (result >= 0) {
        result = a1_decoder_open(A1_MICROPHONE, config, (uint32_t)result, &decoder);
    }
    for (unsigned frame = 0; result >= 0 && frame < 4; ++frame) {
        result = a1_encode(encoder, packet, sizeof(packet), &key);
        if (result > 0) {
            A1Decoded decoded;
            result = a1_decode(decoder, packet, (uint32_t)result, &decoded);
            if (result >= 0) {
                *decoded_samples += decoded.samples;
            }
        }
    }
    a1_codec_close(decoder);
    a1_codec_close(encoder);
    return result;
}
