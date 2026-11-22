// Feed the existing Windows I420 capture pipe into Kyber's pinned avconv
// encoder graph. This probe is an upstream input/encode feasibility step.

#include <libavcodec/avcodec.h>
#include <libavcodec/codec_par.h>
#include <libavutil/error.h>
#include <libavutil/frame.h>
#include <libavutil/opt.h>
#include <libavutil/pixfmt.h>
#include <libavutil/refstruct.h>
#include <libavutil/time.h>
#include <libavconv/avconv.h>
#include <libavconv/dsink.h>
#include <libavconv/dsrc.h>
#include <libavconv/enc.h>
#include <libavconv/params.h>

#include <errno.h>
#include <fcntl.h>
#include <io.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define FRAME_WIDTH 320
#define FRAME_HEIGHT 180
#define FRAME_BYTES ((size_t)FRAME_WIDTH * FRAME_HEIGHT * 3 / 2)

typedef struct ProbeGraph {
    AVConvContext *context;
    AVConvDSrc *source;
    AVConvEncoder *encoder;
    AVConvDSink *sink;
    int started;
} ProbeGraph;

typedef struct Counters {
    uint64_t frames;
    uint64_t packets;
    uint64_t packet_bytes;
    uint64_t keyframes;
    uint64_t config_bytes;
    uint64_t config_records;
    int emit_records;
} Counters;

// Record header: type u8, PTS in microseconds u64 LE, key flag u8, length u32 LE.
// Type 1 contains H.264 codec extradata; type 2 contains one encoded media packet.
static int write_record(uint8_t type, uint64_t pts, int is_key,
                        const uint8_t *payload, uint32_t size) {
    uint8_t header[14];
    header[0] = type;
    for (unsigned i = 0; i < 8; ++i) header[1 + i] = (uint8_t)(pts >> (i * 8));
    header[9] = is_key ? 1 : 0;
    for (unsigned i = 0; i < 4; ++i) header[10 + i] = (uint8_t)(size >> (i * 8));
    if (fwrite(header, 1, sizeof(header), stdout) != sizeof(header) ||
        (size && fwrite(payload, 1, size, stdout) != size)) {
        perror("writing encoded video record");
        return -1;
    }
    return 0;
}

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
                perror("reading camera frame stream");
                return -1;
            }
            if (allow_clean_eof && offset == 0 && feof(stdin)) return 0;
            fprintf(stderr, "truncated camera frame record\n");
            return -1;
        }
        offset += count;
    }
    return 1;
}

static int receive_packets(ProbeGraph *graph, Counters *counters) {
    for (;;) {
        AVConvDSinkDataPacket *data = NULL;
        int status = avconv_dsink_receive_packet(graph->sink, &data, 0);
        if (status == AVERROR(EAGAIN) || status == AVERROR_EOF) return 0;
        if (status < 0) {
            report_av_error("avconv_dsink_receive_packet", status);
            return -1;
        }
        if (data && data->type == AVCONV_DSINK_PACKET_PARAMS && data->par &&
            data->par->codec_par) {
            const AVCodecParameters *params = data->par->codec_par;
            if (params->extradata_size <= 0 || !params->extradata) {
                fprintf(stderr, "avconv encoder did not provide H.264 codec configuration\n");
                av_refstruct_unref(&data);
                return -1;
            }
            if (counters->emit_records &&
                write_record(1, 0, 0, params->extradata,
                             (uint32_t)params->extradata_size) < 0) {
                av_refstruct_unref(&data);
                return -1;
            }
            counters->config_bytes += (uint64_t)params->extradata_size;
            ++counters->config_records;
        } else if (data && data->type == AVCONV_DSINK_PACKET && data->pkt) {
            ++counters->packets;
            counters->packet_bytes += (uint64_t)data->pkt->size;
            counters->keyframes += (data->pkt->flags & AV_PKT_FLAG_KEY) != 0;
            if (counters->emit_records) {
                int64_t packet_pts = data->pkt->pts;
                int64_t pts_us = packet_pts == AV_NOPTS_VALUE
                    ? (int64_t)(counters->packets * 1000000 / 15)
                    : av_rescale_q(packet_pts, (AVRational){1, 15}, AV_TIME_BASE_Q) + 1;
                if (data->pkt->size < 0 || pts_us < 0 ||
                    (uint64_t)data->pkt->size > UINT32_MAX ||
                    write_record(2, (uint64_t)pts_us,
                                 (data->pkt->flags & AV_PKT_FLAG_KEY) != 0,
                                 data->pkt->data, (uint32_t)data->pkt->size) < 0) {
                    av_refstruct_unref(&data);
                    return -1;
                }
            }
        }
        av_refstruct_unref(&data);
    }
}

static int pump_until_source_ready(ProbeGraph *graph, Counters *counters) {
    for (;;) {
        const AVConvProgress *progress = NULL;
        int status = avconv_wait(graph->context, INT64_MAX, 0, &progress);
        if (status < 0) {
            report_av_error("avconv_wait", status);
            return -1;
        }
        if (progress->nb_dsinks && receive_packets(graph, counters) < 0) return -1;
        for (unsigned i = 0; i < progress->nb_dsrcs; ++i)
            if (progress->dsrcs[i] == graph->source->hdl) return 0;
    }
}

static int pump_until_end(ProbeGraph *graph, Counters *counters) {
    for (;;) {
        const AVConvProgress *progress = NULL;
        int status = avconv_wait(graph->context, INT64_MAX, 0, &progress);
        if (status == AVERROR_EOF) return 0;
        if (status < 0) {
            report_av_error("avconv_wait while flushing", status);
            return -1;
        }
        if (progress->nb_dsinks && receive_packets(graph, counters) < 0) return -1;
    }
}

static int push_frame(ProbeGraph *graph, const uint8_t *pixels,
                      int64_t timestamp_us, Counters *counters) {
    AVFrame *frame = av_frame_alloc();
    int status;
    if (!frame) {
        fprintf(stderr, "could not allocate AVFrame\n");
        return -1;
    }
    frame->format = AV_PIX_FMT_YUV420P;
    frame->width = FRAME_WIDTH;
    frame->height = FRAME_HEIGHT;
    frame->pts = av_rescale_q(timestamp_us, (AVRational){1, 1000000},
                              (AVRational){1, 15});
    frame->time_base = (AVRational){1, 15};
    status = av_frame_get_buffer(frame, 32);
    if (status < 0) {
        report_av_error("av_frame_get_buffer", status);
        av_frame_free(&frame);
        return -1;
    }
    status = av_frame_make_writable(frame);
    if (status < 0) {
        report_av_error("av_frame_make_writable", status);
        av_frame_free(&frame);
        return -1;
    }

    const size_t y_size = (size_t)FRAME_WIDTH * FRAME_HEIGHT;
    const size_t chroma_size = y_size / 4;
    for (int row = 0; row < FRAME_HEIGHT; ++row)
        memcpy(frame->data[0] + row * frame->linesize[0],
               pixels + (size_t)row * FRAME_WIDTH, FRAME_WIDTH);
    for (int row = 0; row < FRAME_HEIGHT / 2; ++row) {
        memcpy(frame->data[1] + row * frame->linesize[1],
               pixels + y_size + (size_t)row * (FRAME_WIDTH / 2), FRAME_WIDTH / 2);
        memcpy(frame->data[2] + row * frame->linesize[2],
               pixels + y_size + chroma_size + (size_t)row * (FRAME_WIDTH / 2),
               FRAME_WIDTH / 2);
    }

    for (;;) {
        status = avconv_dsrc_send_frame(graph->source, frame, 0);
        if (status != AVERROR(EAGAIN)) break;
        if (pump_until_source_ready(graph, counters) < 0) {
            av_frame_free(&frame);
            return -1;
        }
    }
    av_frame_free(&frame);
    if (status < 0) {
        report_av_error("avconv_dsrc_send_frame", status);
        return -1;
    }
    return 0;
}

static void generate_synthetic_frame(uint8_t *pixels, unsigned frame_number) {
    const size_t y_size = (size_t)FRAME_WIDTH * FRAME_HEIGHT;
    const size_t chroma_size = y_size / 4;
    for (int row = 0; row < FRAME_HEIGHT; ++row) {
        for (int column = 0; column < FRAME_WIDTH; ++column)
            pixels[(size_t)row * FRAME_WIDTH + column] =
                (uint8_t)(16 + (column + row + frame_number * 2) % 220);
    }
    memset(pixels + y_size, 96 + frame_number % 32, chroma_size);
    memset(pixels + y_size + chroma_size, 160 - frame_number % 32, chroma_size);
}

static int create_graph(ProbeGraph *graph) {
    int status;
    graph->context = avconv_alloc();
    if (!graph->context) {
        fprintf(stderr, "avconv_alloc failed\n");
        return -1;
    }
    status = avconv_dsrc_create(graph->context, &graph->source,
                                AVCONV_HANDLE_PAYLOAD_FRAME, AVMEDIA_TYPE_VIDEO, 0);
    if (status < 0) goto fail_source;
    status = avconv_dsrc_init(graph->source);
    if (status < 0) goto fail_source;
    status = avconv_enc_create(graph->context, &graph->encoder,
                               avcodec_find_encoder_by_name("libx264"), 0);
    if (status < 0) goto fail_encoder;
    AVCodecContext *codec = graph->encoder->enc_ctx;
    codec->width = FRAME_WIDTH;
    codec->height = FRAME_HEIGHT;
    codec->pix_fmt = AV_PIX_FMT_YUV420P;
    codec->time_base = (AVRational){1, 15};
    codec->framerate = (AVRational){15, 1};
    codec->bit_rate = 500000;
    codec->gop_size = 30;
    codec->max_b_frames = 0;
    status = av_opt_set(codec->priv_data, "preset", "ultrafast", 0);
    if (status < 0) goto fail_options;
    status = av_opt_set(codec->priv_data, "tune", "zerolatency", 0);
    if (status < 0) goto fail_options;
    codec->flags |= AV_CODEC_FLAG_GLOBAL_HEADER;
    status = avconv_enc_init(graph->encoder);
    if (status < 0) goto fail_encoder_init;
    status = avconv_dsink_create(graph->context, &graph->sink,
                                 AVCONV_HANDLE_PAYLOAD_PACKET, AVMEDIA_TYPE_VIDEO, 0);
    if (status < 0) goto fail_sink;
    status = avconv_dsink_init(graph->sink);
    if (status < 0) goto fail_sink_init;
    status = avconv_connect(graph->context, graph->source->hdl,
                            graph->encoder->hdl_in, 0);
    if (status < 0) goto fail_connect_in;
    status = avconv_connect(graph->context, graph->encoder->hdl_out,
                            graph->sink->hdl, 0);
    if (status < 0) goto fail_connect_out;
    status = avconv_start(graph->context, 0);
    if (status < 0) goto fail_start;
    graph->started = 1;
    return 0;

fail_start:
fail_connect_out:
fail_connect_in:
fail_sink_init:
fail_sink:
fail_encoder_init:
fail_options:
fail_encoder:
fail_source:
    report_av_error("creating Kyber avconv graph", status);
    return -1;
}

static void destroy_graph(ProbeGraph *graph) {
    if (!graph->context) return;
    if (graph->started) avconv_stop(graph->context, NULL);
    avconv_free(&graph->context);
}

int main(int argc, char **argv) {
    ProbeGraph graph = {0};
    Counters counters = {0};
    uint8_t timestamp_bytes[8];
    uint8_t pixels[FRAME_BYTES];
    unsigned synthetic_frames = 0;
    int result = 1;

    for (int i = 1; i < argc; ++i) {
        if (strcmp(argv[i], "--emit") == 0) {
            counters.emit_records = 1;
        } else if (strcmp(argv[i], "--synthetic") == 0 && i + 1 < argc) {
            char *end = NULL;
            unsigned long parsed = strtoul(argv[++i], &end, 10);
            if (!end || *end || parsed == 0 || parsed > 9000) {
                fprintf(stderr, "--synthetic requires a frame count from 1 to 9000\n");
                return 64;
            }
            synthetic_frames = (unsigned)parsed;
        } else {
            fprintf(stderr, "usage: probe [--emit] [--synthetic FRAMES]\n");
            return 64;
        }
    }
    if (counters.emit_records) {
        static const uint8_t magic[8] = {'S', 'K', 'Y', 'A', 'V', 'C', '0', '1'};
        if (_setmode(_fileno(stdout), _O_BINARY) == -1 ||
            fwrite(magic, 1, sizeof(magic), stdout) != sizeof(magic)) {
            perror("opening encoded video output");
            return 1;
        }
        if (setvbuf(stdout, NULL, _IONBF, 0) != 0) {
            perror("configuring encoded video stream output");
            return 1;
        }
    }
    if (_setmode(_fileno(stdin), _O_BINARY) == -1) {
        perror("setting binary camera input mode");
        goto done;
    }
    if (create_graph(&graph) < 0) goto done;
    if (synthetic_frames) {
        for (unsigned i = 0; i < synthetic_frames; ++i) {
            generate_synthetic_frame(pixels, i);
            uint64_t timestamp = (uint64_t)i * 1000000 / 15;
            if (push_frame(&graph, pixels, (int64_t)timestamp, &counters) < 0)
                goto done;
            ++counters.frames;
        }
    } else {
        for (;;) {
            int read_status = read_exact(timestamp_bytes, sizeof(timestamp_bytes), 1);
            if (read_status == 0) break;
            if (read_status < 0) goto done;
            read_status = read_exact(pixels, sizeof(pixels), 0);
            if (read_status < 0) goto done;
            uint64_t timestamp = 0;
            for (unsigned i = 0; i < sizeof(timestamp_bytes); ++i)
                timestamp |= (uint64_t)timestamp_bytes[i] << (i * 8);
            if (push_frame(&graph, pixels, (int64_t)timestamp, &counters) < 0)
                goto done;
            ++counters.frames;
        }
    }
    for (;;) {
        int status = avconv_dsrc_send_frame(graph.source, NULL, 0);
        if (status != AVERROR(EAGAIN)) {
            if (status < 0) {
                report_av_error("signaling camera stream end", status);
                goto done;
            }
            break;
        }
        if (pump_until_source_ready(&graph, &counters) < 0) goto done;
    }
    if (pump_until_end(&graph, &counters) < 0) goto done;
    result = counters.frames && counters.packets && counters.config_records ? 0 : 2;
done:
    destroy_graph(&graph);
    fprintf(counters.emit_records ? stderr : stdout,
            "{\"frames\": %llu, \"packets\": %llu, \"encoded_bytes\": %llu, \"keyframes\": %llu, \"codec_config_bytes\": %llu}\n",
            (unsigned long long)counters.frames,
            (unsigned long long)counters.packets,
            (unsigned long long)counters.packet_bytes,
            (unsigned long long)counters.keyframes,
            (unsigned long long)counters.config_bytes);
    return result;
}
