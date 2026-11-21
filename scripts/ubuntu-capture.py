#!/usr/bin/python3
"""Normalize one Ubuntu capture source for the opt-in Skyber gate peer."""

import argparse
import os
import re
import stat
import struct
import sys
import time
import uuid
from contextlib import contextmanager
from typing import Iterator
import gi

gi.require_version("Gst", "1.0")
from gi.repository import Gio, GLib, Gst

PORTAL = "org.freedesktop.portal.Desktop"
ROOT = "/org/freedesktop/portal/desktop"
SCREENCAST = "org.freedesktop.portal.ScreenCast"
PCM_BYTES = 960 * 2 * 4
I420_BYTES = 320 * 180 * 3 // 2


class CaptureError(RuntimeError):
    pass


def call(bus: Gio.DBusConnection, path: str, interface: str, method: str,
         args: GLib.Variant | None = None) -> GLib.Variant:
    return bus.call_sync(PORTAL, path, interface, method, args, None,
                         Gio.DBusCallFlags.NONE, 5000, None)


def request(bus: Gio.DBusConnection, method: str, signature: str, previous: tuple,
            options: dict[str, GLib.Variant], *, timeout: int = 5) -> dict:
    token = "skyber_" + uuid.uuid4().hex
    options["handle_token"] = GLib.Variant("s", token)
    sender = bus.get_unique_name()[1:].replace(".", "_")
    predicted = f"{ROOT}/request/{sender}/{token}"
    request_path = predicted
    result = None
    expired = False
    loop = GLib.MainLoop()

    def response(_conn, _sender, _path, _interface, _signal, parameters) -> None:
        nonlocal result
        result = parameters.unpack()
        loop.quit()

    def on_timeout() -> bool:
        nonlocal expired
        expired = True
        loop.quit()
        return GLib.SOURCE_REMOVE

    subscription = bus.signal_subscribe(
        PORTAL, "org.freedesktop.portal.Request", "Response", predicted,
        None, Gio.DBusSignalFlags.NONE, response,
    )
    timer = GLib.timeout_add_seconds(timeout, on_timeout)
    try:
        request_path, = call(
            bus, ROOT, SCREENCAST, method, GLib.Variant(signature, (*previous, options))
        ).unpack()
        if request_path != predicted:
            raise CaptureError("portal returned an unexpected request handle")
        if result is None:
            loop.run()
        if result is None:
            raise CaptureError(f"ScreenCast {method} timed out")
        code, values = result
        if code == 1:
            raise CaptureError("Screen selection cancelled")
        if code:
            raise CaptureError(f"ScreenCast {method} failed ({code})")
        return values
    finally:
        if result is None:
            try:
                call(bus, request_path, "org.freedesktop.portal.Request", "Close")
            except GLib.Error:
                pass
        if not expired:
            GLib.source_remove(timer)
        bus.signal_unsubscribe(subscription)


@contextmanager
def screen_source() -> Iterator[tuple[int, int]]:
    if not os.getenv("WAYLAND_DISPLAY"):
        raise CaptureError("run the screen source inside the Ubuntu Wayland desktop session")
    bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    values = request(bus, "CreateSession", "(a{sv})", (), {
        "session_handle_token": GLib.Variant("s", "skyber_" + uuid.uuid4().hex),
    })
    session = values["session_handle"]
    fd = None
    try:
        request(bus, "SelectSources", "(oa{sv})", (session,), {
            "types": GLib.Variant("u", 3),
            "multiple": GLib.Variant("b", False),
            "persist_mode": GLib.Variant("u", 0),
        })
        values = request(bus, "Start", "(osa{sv})", (session, ""), {}, timeout=60)
        streams = values["streams"]
        if len(streams) != 1:
            raise CaptureError("expected exactly one selected screen stream")
        reply, descriptors = bus.call_with_unix_fd_list_sync(
            PORTAL, ROOT, SCREENCAST, "OpenPipeWireRemote",
            GLib.Variant("(oa{sv})", (session, {})), GLib.VariantType.new("(h)"),
            Gio.DBusCallFlags.NONE, 5000, None, None,
        )
        fd = descriptors.get(reply.unpack()[0])
        yield fd, streams[0][0]
    finally:
        if fd is not None:
            os.close(fd)
        try:
            call(bus, session, "org.freedesktop.portal.Session", "Close")
        except GLib.Error:
            pass


def pipeline_for(kind: str, device: str | None, screen: tuple[int, int] | None) -> Gst.Pipeline:
    pipeline = Gst.Pipeline.new("skyber-capture")

    def chain(names: tuple[str, ...]) -> list[Gst.Element]:
        elements = []
        for name in names:
            element = Gst.ElementFactory.make(name, "frames" if name == "appsink" else None)
            if element is None:
                raise CaptureError(f"required GStreamer element is unavailable: {name}")
            pipeline.add(element)
            elements.append(element)
        for left, right in zip(elements, elements[1:]):
            if not left.link(right):
                raise CaptureError(f"could not link {left.name} to {right.name}")
        return elements

    if kind == "microphone":
        if device is None or not re.fullmatch(r"alsa_input\.[A-Za-z0-9_.-]+", device) or device.endswith(".monitor"):
            raise CaptureError("select a named ALSA microphone from PipeWire’s audio source list")
        source, _, _, filters, sink = chain(
            ("pulsesrc", "audioconvert", "audioresample", "capsfilter", "appsink")
        )
        source.set_property("device", device)
        filters.set_property("caps", Gst.Caps.from_string(
            "audio/x-raw,format=F32LE,layout=interleaved,rate=48000,channels=2"
        ))
    else:
        convert, _, _, filters, sink = chain(
            ("videoconvert", "videoscale", "videorate", "capsfilter", "appsink")
        )
        filters.set_property("caps", Gst.Caps.from_string(
            "video/x-raw,format=I420,width=320,height=180,framerate=15/1,pixel-aspect-ratio=1/1"
        ))
        if screen is not None:
            source, = chain(("pipewiresrc",))
            fd, node = screen
            source.set_property("fd", fd)
            source.set_property("path", str(node))
            if not source.link(convert):
                raise CaptureError("could not link portal video")
        else:
            if device is None or not re.fullmatch(r"/dev/video[0-9]+", device):
                raise CaptureError("select a V4L2 camera device such as /dev/video0")
            if not stat.S_ISCHR(os.stat(device).st_mode):
                raise CaptureError("camera path must identify a character device")
            source, decode = chain(("v4l2src", "decodebin"))
            source.set_property("device", device)

            def link_video(_decode, pad) -> None:
                caps = pad.get_current_caps()
                if caps and caps.get_structure(0).get_name() == "video/x-raw":
                    pad.link(convert.get_static_pad("sink"))

            decode.connect("pad-added", link_video)
    sink.set_property("sync", False)
    sink.set_property("max-buffers", 2)
    sink.set_property("drop", kind != "microphone")
    return pipeline


def capture(kind: str, duration_ms: int, device: str | None,
            screen: tuple[int, int] | None = None) -> None:
    pipeline = pipeline_for(kind, device, screen)
    sink = pipeline.get_by_name("frames")
    bus = pipeline.get_bus()
    start = None
    helper_start_ns = time.monotonic_ns()
    last_sample = time.monotonic()
    sent = 0
    pending = bytearray()
    try:
        if pipeline.set_state(Gst.State.PLAYING) == Gst.StateChangeReturn.FAILURE:
            raise CaptureError(f"could not start {kind} pipeline")
        while start is None or (time.monotonic() - start) * 1000 < duration_ms:
            message = bus.pop_filtered(Gst.MessageType.ERROR | Gst.MessageType.EOS)
            if message is not None:
                if message.type == Gst.MessageType.ERROR:
                    error, _details = message.parse_error()
                    raise CaptureError(str(error))
                break
            sample = sink.emit("try-pull-sample", 100 * Gst.MSECOND)
            if sample is None:
                if time.monotonic() - last_sample >= 5:
                    raise CaptureError(f"timed out waiting for {kind} data")
                continue
            last_sample = time.monotonic()
            if start is None:
                start = last_sample
            buffer = sample.get_buffer()
            size = buffer.get_size()
            if kind == "microphone":
                if size > 1024 * 1024 or size % 8:
                    raise CaptureError("invalid PCM buffer size")
                pending.extend(buffer.extract_dup(0, size))
                whole = len(pending) // PCM_BYTES * PCM_BYTES
                while whole:
                    timestamp_us = (time.monotonic_ns() - helper_start_ns) // 1000
                    sys.stdout.buffer.write(struct.pack("<Q", timestamp_us))
                    sys.stdout.buffer.write(pending[:PCM_BYTES])
                    del pending[:PCM_BYTES]
                    whole -= PCM_BYTES
                    sent += 1
            else:
                if size != I420_BYTES:
                    raise CaptureError(f"invalid I420 frame size ({size})")
                timestamp_us = (time.monotonic_ns() - helper_start_ns) // 1000
                sys.stdout.buffer.write(struct.pack("<Q", timestamp_us))
                sys.stdout.buffer.write(buffer.extract_dup(0, size))
                sent += 1
            sys.stdout.buffer.flush()
        if not sent:
            raise CaptureError(f"empty {kind} source")
    finally:
        pipeline.set_state(Gst.State.NULL)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("kind", choices=("camera", "microphone", "screen"))
    parser.add_argument("duration_ms", type=int)
    parser.add_argument("--device")
    args = parser.parse_args()
    if not 500 <= args.duration_ms <= 15000:
        parser.error("duration must be 500–15000 ms")
    if (args.kind == "screen") == (args.device is not None):
        parser.error("select a device for camera or microphone; screen uses the consent picker")
    Gst.init(None)
    try:
        if args.kind == "screen":
            with screen_source() as stream:
                capture(args.kind, args.duration_ms, None, stream)
        else:
            capture(args.kind, args.duration_ms, args.device)
    except (CaptureError, GLib.Error, OSError, KeyError) as error:
        sys.exit(f"{args.kind} capture failed: {error}")


if __name__ == "__main__":
    main()
