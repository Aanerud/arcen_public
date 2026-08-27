#!/usr/bin/env python3
"""Minimal Arcen Deck stand-in: handshake, PAM auth, then count media frames.

Credential is read from the environment only and never printed. Output is a
single JSON summary safe to paste into a finding.
"""
import asyncio, hashlib, json, os, ssl, sys, time, uuid
import websockets

HOST = os.environ.get("PROBE_HOST", "203.0.113.10")
PORT = int(os.environ.get("PROBE_PORT", "18443"))
USER = os.environ["PROBE_USER"]
PASS = os.environ["PROBE_PASS"]
WIDTH = int(os.environ.get("PROBE_W", "1800"))
HEIGHT = int(os.environ.get("PROBE_H", "1168"))
COLLECT = float(os.environ.get("PROBE_SECONDS", "20"))
SESSION_LOG_ID = os.environ.get("PROBE_SESSION_LOG_ID", str(uuid.uuid4()))


def hash_password(password: str, challenge: str) -> str:
    return hashlib.sha256(f"{password}:{challenge}".encode()).hexdigest()


async def main() -> int:
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE

    result = {
        "connected": False, "auth_ok": False, "server_hello": None,
        "binary_frames": 0, "binary_bytes": 0, "first_frame_ms": None,
        "text_messages": {}, "errors": [],
    }
    t0 = time.time()
    uri = f"wss://{HOST}:{PORT}/"
    try:
        async with websockets.connect(uri, ssl=ctx, max_size=None,
                                      open_timeout=20, ping_interval=None) as ws:
            result["connected"] = True
            hello_msg = {
                "type": "client_hello", "client_name": "arcen-session-probe",
                "version": "3.0.0", "screen_width": WIDTH, "screen_height": HEIGHT,
                "supports_h264": True, "supports_h265": True, "supports_av1": False,
                "supports_yuv444": True, "supports_audio": False,
                "supports_pen": False, "decoder_backend": "probe",
                "capture_mode": "mirror_all", "picked_monitor_id": -1,
                "picked_monitor_name": "",
                "clipboard_protocol_version": 0,
                "clipboard_text_c2s": False, "clipboard_text_s2c": False,
                "clipboard_image_c2s": False, "clipboard_image_s2c": False,
                "input_protocol_version": 2,
                "device_capabilities": {},
                "session_log_id": SESSION_LOG_ID,
            }
            hello = json.dumps(hello_msg)
            async def send_quality(server_codec="h265"):
                await ws.send(json.dumps({
                    "type": "quality_settings",
                    "quality_bias": 0.5, "max_fps": 60,
                    "max_bandwidth_mbps": 50.0,
                    "codec": server_codec or "h265",
                    "chroma": "yuv444",
                    "force_lossless": False, "intra_refresh": False,
                    "enable_audio": False, "audio_bitrate_kbps": 128,
                }))
            # The host speaks first with auth_request. Sending client_hello
            # eagerly makes the host parse it as the auth_response.
            try:
                first = await asyncio.wait_for(ws.recv(), timeout=5)
                pending = first
            except asyncio.TimeoutError:
                await ws.send(hello)
                pending = None
            hello_sent = pending is None

            auth_deadline = time.time() + 30
            while time.time() < auth_deadline and not result["auth_ok"]:
                if pending is not None:
                    raw, pending = pending, None
                else:
                    raw = await asyncio.wait_for(ws.recv(), timeout=30)
                if isinstance(raw, bytes):
                    continue
                msg = json.loads(raw)
                mt = msg.get("type", "?")
                result["text_messages"][mt] = result["text_messages"].get(mt, 0) + 1
                if mt == "auth_request":
                    challenge = msg.get("challenge", "")
                    await ws.send(json.dumps({
                        "type": "auth_response", "method": "pam",
                        "username": USER,
                        "credential": PASS,  # PAM takes the plaintext; TLS is the confidentiality boundary
                        "screen_width": WIDTH, "screen_height": HEIGHT,
                        "monitors": [],
                        "session_log_id": SESSION_LOG_ID,
                    }))

                elif mt == "auth_result":
                    if msg.get("success") or msg.get("ok"):
                        result["auth_ok"] = True
                    else:
                        result["errors"].append(
                            f"auth_result denied: {msg.get('message') or msg.get('reason')}")
                        return finish(result, t0)
                elif mt == "server_hello":
                    result["auth_ok"] = True
                    if not hello_sent:
                        await ws.send(hello)
                        await send_quality(msg.get("codec", "h265"))
                        hello_sent = True
                    result["server_hello"] = {
                        k: msg.get(k) for k in
                        ("screen_width", "screen_height", "codec",
                         "encoder_backend", "encoder_class", "supports_display_update",
                         "session_type", "os_user")}

            if not result["auth_ok"]:
                result["errors"].append("no auth_result/server_hello before deadline")
                return finish(result, t0)

            collect_until = time.time() + COLLECT
            while time.time() < collect_until:
                try:
                    raw = await asyncio.wait_for(
                        ws.recv(), timeout=max(0.5, collect_until - time.time()))
                except asyncio.TimeoutError:
                    break
                if isinstance(raw, bytes):
                    if result["binary_frames"] == 0:
                        result["first_frame_ms"] = round((time.time() - t0) * 1000)
                    result["binary_frames"] += 1
                    result["binary_bytes"] += len(raw)
                else:
                    msg = json.loads(raw)
                    mt = msg.get("type", "?")
                    result["text_messages"][mt] = result["text_messages"].get(mt, 0) + 1
                    if mt == "server_hello":
                        if result["server_hello"] is None:
                            result["server_hello"] = {
                                k: msg.get(k) for k in
                                ("screen_width", "screen_height", "codec",
                                 "encoder_backend", "encoder_class", "supports_display_update",
                                 "session_type", "os_user")}
                        if not hello_sent:
                            await ws.send(hello)
                            await send_quality(msg.get("codec", "h265"))
                            hello_sent = True
    except Exception as exc:  # noqa: BLE001 - probe reports, never raises
        result["errors"].append(f"{type(exc).__name__}: {exc}")
    return finish(result, t0)


def finish(result, t0):
    result["elapsed_s"] = round(time.time() - t0, 1)
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0 if result["binary_frames"] > 0 else 1


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
