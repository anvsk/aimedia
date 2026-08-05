#!/usr/bin/env python3
"""Small obs-websocket v5 controller used by the external interop harness."""

import argparse
import asyncio
import base64
import json
import pathlib
import sys
import uuid

import websockets


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


async def create_media_scene(client: ObsClient, source: str, local: bool):
    await client.request("CreateScene", {"sceneName": "Program"})
    await client.request("SetCurrentProgramScene", {"sceneName": "Program"})
    await client.request(
        "CreateInput",
        {
            "sceneName": "Program",
            "inputName": "Media",
            "inputKind": "ffmpeg_source",
            "inputSettings": {
                ("local_file" if local else "input"): source,
                "input_format": "mpegts",
                "is_local_file": local,
                "looping": local,
                "restart_on_activate": True,
                "buffering_mb": 2,
            },
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
    await create_media_scene(client, args.source, local=False)
    deadline = asyncio.get_running_loop().time() + args.duration
    media_state = None
    image = b""
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
            # A blank 320x180 scene compresses to only a few hundred bytes.
            # The SMPTE bars used by the harness are safely above this bound.
            if image.startswith(b"\x89PNG\r\n\x1a\n") and len(image) >= 1000:
                break
        await asyncio.sleep(0.5)
    if len(image) < 1000:
        raise RuntimeError(
            f"OBS media source did not render a non-blank frame: {media_state}"
        )
    pathlib.Path(args.screenshot).write_bytes(image)
    print(json.dumps({"mediaState": media_state["mediaState"], "screenshotBytes": len(image)}))


async def produce(args):
    client = await ObsClient.connect(args.websocket)
    service = await client.request("GetStreamServiceSettings")
    outputs = await client.request("GetOutputList")
    print(json.dumps({"service": service, "outputs": outputs}, separators=(",", ":")))
    await create_media_scene(client, args.source, local=True)
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
