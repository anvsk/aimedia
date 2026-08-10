#!/usr/bin/env python3
"""Small obs-websocket v5 controller used by the external interop harness."""

import argparse
import asyncio
import base64
import json
import pathlib
import struct
import sys
import uuid
import zlib

import websockets


def png_distinct_colors(image: bytes, limit: int = 16) -> int:
    """Return sampled RGB diversity for an 8-bit, non-interlaced PNG."""
    if not image.startswith(b"\x89PNG\r\n\x1a\n"):
        return 0
    offset = 8
    width = height = bit_depth = color_type = interlace = None
    compressed = bytearray()
    while offset + 12 <= len(image):
        length = struct.unpack(">I", image[offset : offset + 4])[0]
        chunk_type = image[offset + 4 : offset + 8]
        data_start = offset + 8
        data_end = data_start + length
        if data_end + 4 > len(image):
            return 0
        data = image[data_start:data_end]
        if chunk_type == b"IHDR":
            width, height, bit_depth, color_type, _, _, interlace = struct.unpack(
                ">IIBBBBB", data
            )
        elif chunk_type == b"IDAT":
            compressed.extend(data)
        elif chunk_type == b"IEND":
            break
        offset = data_end + 4
    if bit_depth != 8 or interlace != 0 or color_type not in {0, 2, 4, 6}:
        return 0
    channels = {0: 1, 2: 3, 4: 2, 6: 4}[color_type]
    try:
        raw = zlib.decompress(bytes(compressed))
    except zlib.error:
        return 0
    stride = width * channels
    expected = height * (stride + 1)
    if len(raw) != expected:
        return 0

    def paeth(left: int, up: int, upper_left: int) -> int:
        estimate = left + up - upper_left
        left_distance = abs(estimate - left)
        up_distance = abs(estimate - up)
        upper_left_distance = abs(estimate - upper_left)
        if left_distance <= up_distance and left_distance <= upper_left_distance:
            return left
        if up_distance <= upper_left_distance:
            return up
        return upper_left

    previous = bytearray(stride)
    colors = set()
    row_step = max(1, height // 64)
    column_step = max(1, width // 64)
    for row_index in range(height):
        row_start = row_index * (stride + 1)
        filter_type = raw[row_start]
        filtered = raw[row_start + 1 : row_start + 1 + stride]
        row = bytearray(stride)
        for index, value in enumerate(filtered):
            left = row[index - channels] if index >= channels else 0
            up = previous[index]
            upper_left = previous[index - channels] if index >= channels else 0
            if filter_type == 0:
                predictor = 0
            elif filter_type == 1:
                predictor = left
            elif filter_type == 2:
                predictor = up
            elif filter_type == 3:
                predictor = (left + up) // 2
            elif filter_type == 4:
                predictor = paeth(left, up, upper_left)
            else:
                return 0
            row[index] = (value + predictor) & 0xFF
        if row_index % row_step == 0:
            for column in range(0, width, column_step):
                pixel = column * channels
                if color_type in {0, 4}:
                    rgb = (row[pixel],) * 3
                else:
                    rgb = tuple(row[pixel : pixel + 3])
                colors.add(rgb)
                if len(colors) >= limit:
                    return len(colors)
        previous = row
    return len(colors)


class ObsClient:
    def __init__(self, socket):
        self.socket = socket

    @classmethod
    async def connect(cls, uri: str, timeout: float = 20.0):
        deadline = asyncio.get_running_loop().time() + timeout
        while True:
            try:
                socket = await websockets.connect(uri, open_timeout=2)
                hello = json.loads(await socket.recv())
                if hello.get("op") != 0:
                    raise RuntimeError(f"unexpected OBS hello: {hello}")
                if hello["d"].get("authentication"):
                    raise RuntimeError("the test OBS websocket must have authentication disabled")
                await socket.send(
                    json.dumps({"op": 1, "d": {"rpcVersion": 1, "eventSubscriptions": 64}})
                )
                identified = json.loads(await socket.recv())
                if identified.get("op") != 2:
                    raise RuntimeError(f"OBS identification failed: {identified}")
                # The WebSocket listener starts just before the OBS frontend reports
                # its final ready state. Avoid racing the first scene mutation.
                await asyncio.sleep(2)
                return cls(socket)
            except (OSError, asyncio.TimeoutError, websockets.WebSocketException):
                if asyncio.get_running_loop().time() >= deadline:
                    raise RuntimeError("OBS websocket did not become ready")
                await asyncio.sleep(0.5)

    async def request(self, request_type: str, request_data=None):
        request_id = str(uuid.uuid4())
        await self.socket.send(
            json.dumps(
                {
                    "op": 6,
                    "d": {
                        "requestType": request_type,
                        "requestId": request_id,
                        "requestData": request_data or {},
                    },
                }
            )
        )
        while True:
            message = json.loads(await self.socket.recv())
            if message.get("op") == 5:
                print(json.dumps(message, separators=(",", ":")), file=sys.stderr)
                continue
            if message.get("op") != 7 or message["d"].get("requestId") != request_id:
                continue
            status = message["d"]["requestStatus"]
            if not status.get("result"):
                raise RuntimeError(
                    f"OBS {request_type} failed with {status.get('code')}: "
                    f"{status.get('comment', 'no details')}"
                )
            return message["d"].get("responseData", {})


async def create_media_scene(
    client: ObsClient, source: str, local: bool, input_format: str
):
    input_settings = {
        ("local_file" if local else "input"): source,
        "is_local_file": local,
        "looping": local,
        "restart_on_activate": True,
        "buffering_mb": 2,
    }
    if input_format:
        input_settings["input_format"] = input_format
    await client.request("CreateScene", {"sceneName": "Program"})
    await client.request("SetCurrentProgramScene", {"sceneName": "Program"})
    await client.request(
        "CreateInput",
        {
            "sceneName": "Program",
            "inputName": "Media",
            "inputKind": "ffmpeg_source",
            "inputSettings": input_settings,
            "sceneItemEnabled": True,
        },
    )


async def wait_for(
    client: ObsClient,
    request_type: str,
    predicate,
    timeout: float = 20.0,
    request_data=None,
):
    deadline = asyncio.get_running_loop().time() + timeout
    while True:
        state = await client.request(request_type, request_data)
        if predicate(state):
            return state
        if asyncio.get_running_loop().time() >= deadline:
            raise RuntimeError(f"OBS {request_type} did not reach the expected state: {state}")
        await asyncio.sleep(0.5)


async def consume(args):
    client = await ObsClient.connect(args.websocket)
    await create_media_scene(
        client, args.source, local=False, input_format=args.input_format
    )
    deadline = asyncio.get_running_loop().time() + args.duration
    media_state = None
    image = b""
    distinct_colors = 0
    while asyncio.get_running_loop().time() < deadline:
        media_state = await client.request("GetMediaInputStatus", {"inputName": "Media"})
        if media_state.get("mediaState") in {
            "OBS_MEDIA_STATE_PLAYING",
            "OBS_MEDIA_STATE_OPENING",
        }:
            screenshot = await client.request(
                "GetSourceScreenshot",
                {
                    "sourceName": "Program",
                    "imageFormat": "png",
                    "imageWidth": 320,
                    "imageHeight": 180,
                    "imageCompressionQuality": 50,
                },
            )
            encoded = screenshot["imageData"].split(",", 1)[-1]
            image = base64.b64decode(encoded)
            distinct_colors = png_distinct_colors(image)
            if distinct_colors >= 4:
                break
        await asyncio.sleep(0.5)
    if image:
        pathlib.Path(args.screenshot).write_bytes(image)
    if distinct_colors < 4:
        raise RuntimeError(
            "OBS media source did not render a non-blank frame: "
            f"state={media_state} screenshotBytes={len(image)} "
            f"distinctColors={distinct_colors}"
        )
    print(
        json.dumps(
            {
                "mediaState": media_state["mediaState"],
                "screenshotBytes": len(image),
                "distinctColors": distinct_colors,
            }
        )
    )


async def produce(args):
    client = await ObsClient.connect(args.websocket)
    service = await client.request("GetStreamServiceSettings")
    outputs = await client.request("GetOutputList")
    print(json.dumps({"service": service, "outputs": outputs}, separators=(",", ":")))
    await create_media_scene(client, args.source, local=True, input_format="mpegts")
    await wait_for(
        client,
        "GetMediaInputStatus",
        lambda state: state.get("mediaState") == "OBS_MEDIA_STATE_PLAYING",
        request_data={"inputName": "Media"},
    )
    await client.request("StartStream")
    await wait_for(
        client,
        "GetStreamStatus",
        lambda state: state.get("outputActive") is True,
    )
    await asyncio.sleep(args.duration)
    await client.request("StopStream")
    print(json.dumps({"streamedSeconds": args.duration, "destination": args.destination}))


def parse_args():
    parser = argparse.ArgumentParser()
    parser.add_argument("--websocket", default="ws://127.0.0.1:4455")
    subparsers = parser.add_subparsers(dest="command", required=True)

    consume_parser = subparsers.add_parser("consume")
    consume_parser.add_argument("--source", required=True)
    consume_parser.add_argument("--input-format", default="mpegts")
    consume_parser.add_argument("--screenshot", required=True)
    consume_parser.add_argument("--duration", type=float, default=5)

    produce_parser = subparsers.add_parser("produce")
    produce_parser.add_argument("--source", required=True)
    produce_parser.add_argument("--destination", required=True)
    produce_parser.add_argument("--duration", type=float, default=8)
    return parser.parse_args()


async def main():
    args = parse_args()
    if args.command == "consume":
        await consume(args)
    else:
        await produce(args)


if __name__ == "__main__":
    asyncio.run(main())
