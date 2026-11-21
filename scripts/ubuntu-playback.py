#!/usr/bin/python3
"""Play decoded Skyber gate frames in the Ubuntu desktop session."""

import argparse
import os
import struct
import sys
import threading
import time

import gi

gi.require_version("Gst", "1.0")
from gi.repository import GLib, Gst

WIDTH = 320
HEIGHT = 180
VIDEO_BYTES = WIDTH * HEIGHT * 3 // 2
AUDIO_BYTES = 960 * 4


def read_exact(fd: int, size: int, *, allow_eof: bool = False) -> bytes | None:
    data = bytearray()
    while len(data) < size:
        block = os.read(fd, size - len(data))
        if not block:
            if allow_eof and not data:
                return None
            raise RuntimeError("incomplete video presentation record")
        data.extend(block)
    return bytes(data)


def build_pipeline(factories: tuple[str, ...]) -> tuple[Gst.Pipeline, list[Gst.Element]]:
    pipeline = Gst.Pipeline.new("skyber-playback")
    elements = []
    for factory in factories:
        element = Gst.ElementFactory.make(factory)
        if element is None:
            raise RuntimeError(f"missing GStreamer element: {factory}")
        pipeline.add(element)
        if elements and not elements[-1].link(element):
            raise RuntimeError(f"cannot link {factory}")
        elements.append(element)
    return pipeline, elements


def feed(source: Gst.Element, data: bytes, pts: int, duration: int) -> None:
    buffer = Gst.Buffer.new_wrapped(data)
    buffer.pts = pts
    buffer.duration = duration
    if source.emit("push-buffer", buffer) != Gst.FlowReturn.OK:
        raise RuntimeError("GStreamer rejected playback data")


def bus_result(bus: Gst.Bus, timeout: int = 0) -> Gst.Message | None:
    message = bus.timed_pop_filtered(timeout, Gst.MessageType.ERROR | Gst.MessageType.EOS)
    if message is not None and message.type == Gst.MessageType.ERROR:
        error, _details = message.parse_error()
        raise RuntimeError(str(error))
    return message


def audio(device: str | None) -> None:
    pipeline, (source, volume, sink) = build_pipeline(("appsrc", "volume", "pulsesink"))
    source.set_property("caps", Gst.Caps.from_string(
        "audio/x-raw,format=S16LE,layout=interleaved,channels=2,rate=48000"
    ))
    source.set_property("format", Gst.Format.TIME)
    source.set_property("block", True)
    source.set_property("max-bytes", 2 * AUDIO_BYTES)
    volume.set_property("volume", 0.2)
    if device:
        sink.set_property("device", device)
    bus = pipeline.get_bus()
    samples = 0
    try:
        if pipeline.set_state(Gst.State.PLAYING) == Gst.StateChangeReturn.FAILURE:
            raise RuntimeError("cannot start audio output")
        while block := sys.stdin.buffer.read(AUDIO_BYTES):
            bus_result(bus)
            if len(block) % 4:
                raise RuntimeError("incomplete stereo PCM frame")
            count = len(block) // 4
            pts = samples * Gst.SECOND // 48000
            end = (samples + count) * Gst.SECOND // 48000
            feed(source, block, pts, end - pts)
            samples += count
        if source.emit("end-of-stream") != Gst.FlowReturn.OK:
            raise RuntimeError("cannot finish audio output")
        if bus_result(bus, 4 * Gst.SECOND) is None or not samples:
            raise RuntimeError("audio output did not drain")
        print(f"Played {samples} stereo sample frames", file=sys.stderr, flush=True)
    finally:
        pipeline.set_state(Gst.State.NULL)


def video(track: str) -> int:
    gi.require_version("Gtk", "4.0")
    from gi.repository import Gdk, Gtk

    pipeline, (source, _, sink) = build_pipeline(("appsrc", "videoconvert", "appsink"))
    source.set_property("caps", Gst.Caps.from_string(
        f"video/x-raw,format=I420,width={WIDTH},height={HEIGHT},framerate=15/1"
    ))
    source.set_property("format", Gst.Format.TIME)
    source.set_property("block", True)
    source.set_property("max-bytes", 2 * VIDEO_BYTES)
    sink.set_property("caps", Gst.Caps.from_string("video/x-raw,format=RGBA"))
    sink.set_property("sync", False)
    sink.set_property("max-buffers", 2)
    sink.set_property("drop", True)

    lock = threading.Lock()
    latest: tuple[bytes, int] | None = None
    finished = False
    error = None
    presented = 0
    helper_start_ns = time.monotonic_ns()
    app = Gtk.Application()

    def read_frames() -> None:
        nonlocal latest, finished, error
        bus = pipeline.get_bus()
        try:
            if pipeline.set_state(Gst.State.PLAYING) == Gst.StateChangeReturn.FAILURE:
                raise RuntimeError("cannot start video conversion")
            index = 0
            # A closed window can exit while this worker waits on stdin.
            # Use the file descriptor directly, avoiding a buffered stream lock.
            fd = sys.stdin.fileno()
            while header := read_exact(fd, 8, allow_eof=True):
                packet_id = struct.unpack("<Q", header)[0]
                frame = read_exact(fd, VIDEO_BYTES)
                if frame is None:
                    raise RuntimeError("missing video frame payload")
                bus_result(bus)
                feed(source, frame, index * Gst.SECOND // 15, Gst.SECOND // 15)
                index += 1
                sample = sink.emit("try-pull-sample", 2 * Gst.SECOND)
                if sample is None:
                    bus_result(bus)
                    raise RuntimeError("video conversion timed out")
                converted = sample.get_buffer()
                if converted.get_size() != WIDTH * HEIGHT * 4:
                    raise RuntimeError("unexpected RGBA stride")
                with lock:
                    latest = (converted.extract_dup(0, converted.get_size()), packet_id)
        except (OSError, RuntimeError) as failure:
            with lock:
                error = str(failure)
        finally:
            pipeline.set_state(Gst.State.NULL)
            with lock:
                finished = True

    def activate(application: Gtk.Application) -> None:
        window = Gtk.ApplicationWindow(application=application, title=f"Skyber remote {track}")
        window.set_default_size(440, 280)
        picture = Gtk.Picture()
        picture.set_content_fit(Gtk.ContentFit.CONTAIN)
        window.set_child(picture)
        window.present()
        pending_packet_id: int | None = None
        pending_generation = 0
        reported_generation = 0

        def after_paint(_clock: Gdk.FrameClock) -> None:
            nonlocal pending_packet_id, reported_generation, presented
            if pending_packet_id is None or reported_generation == pending_generation:
                return
            helper_timestamp_us = (time.monotonic_ns() - helper_start_ns) // 1000
            print(f"SKYBER_PRESENT_VIDEO {pending_packet_id} {helper_timestamp_us}",
                  file=sys.stderr, flush=True)
            pending_packet_id = None
            reported_generation = pending_generation
            presented += 1

        frame_clock = window.get_frame_clock()
        if frame_clock is None:
            raise RuntimeError("GTK window has no frame clock")
        frame_clock.connect("after-paint", after_paint)

        def update() -> bool:
            nonlocal latest, pending_packet_id, pending_generation
            with lock:
                image, latest = latest, None
                done = finished
            if image is not None:
                pixels, packet_id = image
                texture = Gdk.MemoryTexture.new(
                    WIDTH, HEIGHT, Gdk.MemoryFormat.R8G8B8A8,
                    GLib.Bytes.new(pixels), WIDTH * 4,
                )
                picture.set_paintable(texture)
                pending_packet_id = packet_id
                pending_generation += 1
            if done:
                if reported_generation == pending_generation:
                    application.quit()
                    return GLib.SOURCE_REMOVE
            return GLib.SOURCE_CONTINUE

        GLib.timeout_add(16, update)
        threading.Thread(target=read_frames, daemon=True).start()

    app.connect("activate", activate)
    status = app.run([])
    with lock:
        if error:
            raise RuntimeError(error)
        if finished and not presented:
            raise RuntimeError("video output presented no frames")
    print(f"Presented {presented} {track} frames", file=sys.stderr, flush=True)
    return status


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("track", choices=("microphone", "camera", "screen"))
    parser.add_argument("device", nargs="?")
    args = parser.parse_args()
    if args.track != "microphone" and args.device is not None:
        parser.error("an output device is only accepted for audio")
    Gst.init(None)
    try:
        if args.track == "microphone":
            audio(args.device)
        else:
            sys.exit(video(args.track))
    except (RuntimeError, OSError) as error:
        sys.exit(f"{args.track} playback failed: {error}")


if __name__ == "__main__":
    main()
