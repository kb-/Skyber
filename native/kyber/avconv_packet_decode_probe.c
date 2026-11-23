// Decode the avconv frame-source probe's bounded record stream using Kyber's
// pinned public avconv decoder graph API.

#include <libavcodec/avcodec.h>
#include <libavcodec/codec_par.h>
#include <libavutil/error.h>
#include <libavutil/frame.h>
#include <libavutil/mem.h>
#include <libavutil/refstruct.h>
#include <libavutil/time.h>
#include <libavconv/avconv.h>
#include <libavconv/dec.h>
#include <libavconv/dsink.h>
#include <libavconv/dsrc.h>
#include <libavconv/params.h>

#include <errno.h>
#include <fcntl.h>
#include <io.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define MAX_CODEC_CONFIG (1024U * 1024U)
#define MAX_MEDIA_PACKET (4U * 1024U * 1024U)
#define MAX_MEDIA_PACKETS 9000U

typedef struct DecodeGraph {
    AVConvContext *context;
    AVConvDSrc *source;
    AVConvDecoder *decoder;
    AVConvDSink *sink;
    int started;
} DecodeGraph;

typedef struct DecodeStats {
    uint64_t packets;
    uint64_t frames;
    uint64_t last_pts;
    uint64_t checksum;
} DecodeStats;

static void report_av_error(const char *operation, int status) {
    char message[AV_ERROR_MAX_STRING_SIZE] = {0};
    av_strerror(status, message, sizeof(message));
    fprintf(stderr, "%s: %s\n", operation, message);
}

static int read_exact(uint8_t *buffer, size_t size, int allow_clean_eof) {
    size_t offset = 0;
    while (offset < size) {
        size_t count = fread(buffer + offset, 1, size - offset, stdin);
        if (count == 0) {
            if (ferror(stdin)) {
                perror("reading avconv packet stream");
                return -1;
            }
            if (allow_clean_eof && offset == 0 && feof(stdin)) return 0;
            fprintf(stderr, "truncated avconv packet record\n");
            return -1;
        }
        offset += count;
    }
    return 1;
}

static int drain_frames(DecodeGraph *graph, DecodeStats *stats) {
    for (;;) {
        AVConvDSinkDataFrame *data = NULL;
        int status = avconv_dsink_receive_frame(graph->sink, &data, 0);
        if (status == AVERROR(EAGAIN) || status == AVERROR_EOF) return 0;
        if (status < 0) {
            report_av_error("avconv_dsink_receive_frame", status);
            return -1;
        }
        if (data && data->type == AVCONV_DSINK_FRAME && data->frame) {
            const AVFrame *frame = data->frame;
            if (frame->width <= 0 || frame->height <= 0 || frame->linesize[0] <= 0 ||
                !frame->data[0]) {
                fprintf(stderr, "avconv decoder returned an invalid video frame\n");
                av_refstruct_unref(&data);
                return -1;
            }
            ++stats->frames;
            if (frame->pts != AV_NOPTS_VALUE && frame->pts >= 0)
                stats->last_pts = (uint64_t)frame->pts;
            for (int row = 0; row < frame->height; row += 3) {
                for (int column = 0; column < frame->width; column += 97) {
                    size_t offset = (size_t)row * (size_t)frame->linesize[0]
                        + (size_t)column;
                    stats->checksum = (stats->checksum * 33U) ^ frame->data[0][offset];
                }
            }
        }
        av_refstruct_unref(&data);
    }
}

static int wait_for_source(DecodeGraph *graph, DecodeStats *stats) {
    for (;;) {
        const AVConvProgress *progress = NULL;
        int status = avconv_wait(graph->context, INT64_MAX, 0, &progress);
        if (status < 0) {
            report_av_error("avconv_wait", status);
            return -1;
        }
        if (progress->nb_dsinks && drain_frames(graph, stats) < 0) return -1;
        for (unsigned i = 0; i < progress->nb_dsrcs; ++i)
            if (progress->dsrcs[i] == graph->source->hdl) return 0;
    }
}

static int pump_to_end(DecodeGraph *graph, DecodeStats *stats) {
    for (;;) {
        const AVConvProgress *progress = NULL;
        int status = avconv_wait(graph->context, INT64_MAX, 0, &progress);
        if (status == AVERROR_EOF) return drain_frames(graph, stats);
        if (status < 0) {
            report_av_error("avconv_wait while draining", status);
            return -1;
        }
        if (progress->nb_dsinks && drain_frames(graph, stats) < 0) return -1;
    }
}

static int create_graph(DecodeGraph *graph) {
    int status;
    const AVCodec *h264_decoder = avcodec_find_decoder_by_name("h264");
    if (!h264_decoder) {
        fprintf(stderr, "pinned avconv build has no H.264 software decoder\n");
        return -1;
    }
    graph->context = avconv_alloc();
    if (!graph->context) {
        fprintf(stderr, "avconv_alloc failed\n");
        return -1;
    }
    status = avconv_dsrc_create(graph->context, &graph->source,
                                AVCONV_HANDLE_PAYLOAD_PACKET, AVMEDIA_TYPE_VIDEO, 0);
    if (status < 0) goto fail;
    status = avconv_dsrc_init(graph->source);
    if (status < 0) goto fail;
    status = avconv_dec_create(graph->context, &graph->decoder,
                               h264_decoder, 0);
    if (status < 0) goto fail;
    graph->decoder->params->flags = AVCONV_DECODER_FLAG_FRAMERATE_FORCED;
    graph->decoder->params->framerate = (AVRational){15, 1};
    status = avconv_dec_init(graph->decoder);
    if (status < 0) goto fail;
    status = avconv_dsink_create(graph->context, &graph->sink,
                                 AVCONV_HANDLE_PAYLOAD_FRAME, AVMEDIA_TYPE_VIDEO, 0);
    if (status < 0) goto fail;
    status = avconv_dsink_init(graph->sink);
    if (status < 0) goto fail;
    status = avconv_connect(graph->context, graph->source->hdl,
                            graph->decoder->hdl_in, 0);
    if (status < 0) goto fail;
    status = avconv_connect(graph->context, graph->decoder->hdl_out,
                            graph->sink->hdl, 0);
    if (status < 0) goto fail;
    status = avconv_start(graph->context, 0);
    if (status < 0) goto fail;
    graph->started = 1;
    return 0;
fail:
    report_av_error("creating Kyber avconv decode graph", status);
    return -1;
}

static void destroy_graph(DecodeGraph *graph) {
    if (!graph->context) return;
    if (graph->started) avconv_stop(graph->context, NULL);
    avconv_free(&graph->context);
}

static int submit_codec_params(DecodeGraph *graph, const uint8_t *data,
                              uint32_t size, DecodeStats *stats) {
    AVConvCodecParams *params = avconv_codec_params_alloc();
    if (!params) {
        fprintf(stderr, "could not allocate avconv codec parameters\n");
        return -1;
    }
    if (!params->codec_par) {
        avconv_codec_params_free(&params);
        fprintf(stderr, "could not allocate H.264 codec parameters\n");
        return -1;
    }
    params->codec_par->codec_type = AVMEDIA_TYPE_VIDEO;
    params->codec_par->codec_id = AV_CODEC_ID_H264;
    params->codec_par->width = 320;
    params->codec_par->height = 180;
    params->codec_par->extradata = av_mallocz((size_t)size + AV_INPUT_BUFFER_PADDING_SIZE);
    if (!params->codec_par->extradata) {
        avconv_codec_params_free(&params);
        fprintf(stderr, "could not allocate H.264 codec configuration\n");
        return -1;
    }
    memcpy(params->codec_par->extradata, data, size);
    params->codec_par->extradata_size = (int)size;
    params->time_base = (AVRational){1, 15};
    for (;;) {
        int status = avconv_dsrc_send_codec_params(graph->source, &params, 0);
        if (status != AVERROR(EAGAIN)) {
            if (status < 0) report_av_error("avconv_dsrc_send_codec_params", status);
            if (params) avconv_codec_params_free(&params);
            return status < 0 ? -1 : 0;
        }
        if (wait_for_source(graph, stats) < 0) {
            avconv_codec_params_free(&params);
            return -1;
        }
    }
}

static int submit_packet(DecodeGraph *graph, const uint8_t *data, uint32_t size,
                         uint64_t pts_us, int is_key, DecodeStats *stats) {
    AVPacket *packet = av_packet_alloc();
    if (!packet) {
        fprintf(stderr, "could not allocate H.264 packet\n");
        return -1;
    }
    int status = av_new_packet(packet, (int)size);
    if (status < 0) {
        report_av_error("av_new_packet", status);
        av_packet_free(&packet);
        return -1;
    }
    memcpy(packet->data, data, size);
    packet->pts = av_rescale_q((int64_t)pts_us, (AVRational){1, 1000000},
                               (AVRational){1, 15});
    packet->dts = packet->pts;
    if (is_key) packet->flags |= AV_PKT_FLAG_KEY;
    for (;;) {
        status = avconv_dsrc_send_packet(graph->source, packet, 0);
        if (status != AVERROR(EAGAIN)) break;
        if (wait_for_source(graph, stats) < 0) {
            av_packet_free(&packet);
            return -1;
        }
    }
    if (status < 0) {
        report_av_error("avconv_dsrc_send_packet", status);
        av_packet_free(&packet);
        return -1;
    }
    ++stats->packets;
    return 0;
}

int main(void) {
    static const uint8_t expected_magic[8] = {'S', 'K', 'Y', 'A', 'V', 'C', '0', '1'};
    DecodeGraph graph = {0};
    DecodeStats stats = {0};
    uint8_t magic[8];
    int result = 1;
    int got_config = 0;
    uint64_t last_packet_pts = 0;

    if (_setmode(_fileno(stdin), _O_BINARY) == -1 ||
        _setmode(_fileno(stdout), _O_BINARY) == -1) {
        perror("setting binary probe streams");
        return 1;
    }
    if (read_exact(magic, sizeof(magic), 0) < 0 ||
        memcmp(magic, expected_magic, sizeof(magic)) != 0) {
        fprintf(stderr, "invalid avconv packet stream signature\n");
        goto done;
    }
    if (create_graph(&graph) < 0) goto done;
    for (;;) {
        uint8_t header[14];
        int read_status = read_exact(header, sizeof(header), 1);
        if (read_status == 0) break;
        if (read_status < 0) goto done;
        uint8_t type = header[0];
        uint64_t pts_us = 0;
        for (unsigned i = 0; i < 8; ++i) pts_us |= (uint64_t)header[1 + i] << (i * 8);
        if (header[9] > 1 || (type != 1 && type != 2)) {
            fprintf(stderr, "invalid avconv packet record header\n");
            goto done;
        }
        uint32_t size = 0;
        for (unsigned i = 0; i < 4; ++i) size |= (uint32_t)header[10 + i] << (i * 8);
        uint32_t limit = type == 1 ? MAX_CODEC_CONFIG : MAX_MEDIA_PACKET;
        if (!size || size > limit) {
            fprintf(stderr, "avconv packet record exceeds its input bound\n");
            goto done;
        }
        uint8_t *payload = malloc(size);
        if (!payload) {
            fprintf(stderr, "could not allocate bounded avconv record\n");
            goto done;
        }
        read_status = read_exact(payload, size, 0);
        if (read_status < 0) {
            free(payload);
            goto done;
        }
        if (type == 1) {
            if (got_config || stats.packets || pts_us || header[9]) {
                fprintf(stderr, "unexpected avconv codec configuration record\n");
                free(payload);
                goto done;
            }
            if (submit_codec_params(&graph, payload, size, &stats) < 0) {
                free(payload);
                goto done;
            }
            got_config = 1;
        } else {
            if (!got_config || stats.packets >= MAX_MEDIA_PACKETS ||
                pts_us <= last_packet_pts || pts_us > UINT64_C(600000000)) {
                fprintf(stderr, "invalid avconv media packet order or timestamp\n");
                free(payload);
                goto done;
            }
            if (submit_packet(&graph, payload, size, pts_us, header[9] != 0, &stats) < 0) {
                free(payload);
                goto done;
            }
            last_packet_pts = pts_us;
        }
        free(payload);
    }
    if (!got_config || !stats.packets) {
        fprintf(stderr, "avconv packet stream contained no configured video\n");
        goto done;
    }
    for (;;) {
        int status = avconv_dsrc_send_packet(graph.source, NULL, 0);
        if (status != AVERROR(EAGAIN)) {
            if (status < 0) report_av_error("flushing avconv decoder", status);
            if (status < 0) goto done;
            break;
        }
        if (wait_for_source(&graph, &stats) < 0) goto done;
    }
    if (pump_to_end(&graph, &stats) < 0 || stats.frames == 0) goto done;
    printf("{\"decoder_backend\":\"kyber-avconv-h264\",\"packets\":%llu,\"decoded_frames\":%llu,\"last_pts\":%llu,\"frame_checksum\":%llu}\n",
           (unsigned long long)stats.packets,
           (unsigned long long)stats.frames,
           (unsigned long long)stats.last_pts,
           (unsigned long long)stats.checksum);
    result = 0;
done:
    destroy_graph(&graph);
    return result;
}
