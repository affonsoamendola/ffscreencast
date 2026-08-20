"""End-to-end check: negotiate against a running server and count frames."""

import asyncio
import sys

import aiohttp
from aiortc import RTCPeerConnection, RTCSessionDescription

URL = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8080"
PASSWORD = sys.argv[2] if len(sys.argv) > 2 else ""
WANT = 15


async def main() -> int:
    async with aiohttp.ClientSession() as session:
        # Authenticate
        async with session.post(
            f"{URL}/api/auth",
            json={"password": PASSWORD},
        ) as resp:
            if resp.status != 200:
                print(f"FAIL: auth failed (status {resp.status})")
                return 1
            token = (await resp.json())["token"]

        pc = RTCPeerConnection()
        got = asyncio.Queue()

        @pc.on("track")
        def on_track(track):
            async def drain():
                count = 0
                while count < WANT:
                    frame = await track.recv()
                    count += 1
                    if count == 1:
                        print(f"first frame {frame.width}x{frame.height} pts={frame.pts}")
                await got.put(count)

            asyncio.ensure_future(drain())

        pc.addTransceiver("video", direction="recvonly")
        await pc.setLocalDescription(await pc.createOffer())

        async with session.post(
            f"{URL}/offer",
            headers={"Authorization": f"Bearer {token}"},
            json={"sdp": pc.localDescription.sdp, "type": pc.localDescription.type},
        ) as resp:
            answer = await resp.json()

        await pc.setRemoteDescription(RTCSessionDescription(**answer))
        print("negotiated, waiting for frames...")

        try:
            count = await asyncio.wait_for(got.get(), timeout=30)
        except asyncio.TimeoutError:
            print("FAIL: timed out waiting for frames")
            await pc.close()
            return 1

        print(f"PASS: received {count} frames")
        await pc.close()
        return 0


sys.exit(asyncio.run(main()))
