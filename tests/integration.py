"""Rust-only functional checks. Requires Python 3.11+ and the built core CLI.

Inputs and outputs stay in a TemporaryDirectory. No game files, .NET SDK,
third-party Python packages or persistent test reports are required.
"""
from __future__ import annotations

import argparse
import functools
import io
import json
from pathlib import Path
import plistlib
import struct
import subprocess
import tempfile
import threading
import zipfile
import zlib

ROOT = Path(__file__).resolve().parents[1]
GAMES = ("cf1", "cf2", "cf3", "cf4", "abo")
KEYS = {
    "cf1": (0x89427C80, 0xF850A6D9, 0x14FB3BA0, 0x3A557437),
    "cf2": (0x67AA748A, 0xFB868651, 0xE8243360, 0x34062D80),
    "cf3": (0xA8AC7F50, 0x63F379E7, 0x06AE82BA, 0x5405FE14),
    "cf4": (0x3CE64A05, 0x81E437A2, 0x37DB91EC, 0x65FA03B8),
    "plist3": (0x26AB1359, 0x1C2485A3, 0xF2B34691, 0xAA172AF6),
    "plist4": (0x43F21B68, 0x9A0F3610, 0x3AC65312, 0xB8926AA3),
}
KEYS["abo"] = KEYS["cf3"]


@functools.lru_cache(maxsize=16)
def keystream(key: tuple[int, ...], rounds: int) -> list[int]:
    words = [0] * 1024
    z = total = 0
    for _ in range(rounds):
        total = (total + 0x9E3779B9) & 0xFFFFFFFF
        e = (total >> 2) & 3
        for p in range(1024):
            y = words[(p + 1) % 1024]
            mix = (((z >> 5 ^ y << 2) + (y >> 3 ^ z << 4)) ^
                   ((total ^ y) + (key[(p & 3) ^ e] ^ z))) & 0xFFFFFFFF
            z = words[p] = (words[p] + mix) & 0xFFFFFFFF
    return words


def transform(payload: bytes, key: tuple[int, ...], rounds: int, wrapped: bool) -> bytes:
    data = bytearray(payload)
    stream = keystream(key, rounds)
    count = len(data) // 4
    indexes = list(range(min(count, 512)))
    if wrapped:
        middle = list(range(512, max(512, count - 512), 4))
        indexes.extend(middle)
        indexes.extend(range((middle[-1] + 4) if middle else min(count, 512), count))
    else:
        indexes.extend(range(512, count, 64))
    for n, word in enumerate(indexes):
        value = struct.unpack_from("<I", data, word * 4)[0] ^ stream[n % 1024]
        struct.pack_into("<I", data, word * 4, value)
    return bytes(data)


def wrapped(raw: bytes, game: str) -> bytes:
    fourth = game == "cf4"
    signature = b"\xff\xdb\xff\xee\x66" if fourth else b"czzf"
    header = bytearray(20)
    header[:len(signature)] = signature
    struct.pack_into("<I", header, 8, zlib.crc32(raw))
    struct.pack_into("<I", header, 16, len(raw))
    return bytes(header) + transform(zlib.compress(raw, 1),
                                    KEYS["plist4" if fourth else "plist3"],
                                    8 if fourth else 6, True)


def png(width: int, height: int, rgba: bytes) -> bytes:
    def chunk(kind: bytes, payload: bytes) -> bytes:
        return struct.pack(">I", len(payload)) + kind + payload + struct.pack(">I", zlib.crc32(kind + payload))
    scanlines = b"".join(b"\0" + rgba[y * width * 4:(y + 1) * width * 4] for y in range(height))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)) +
            chunk(b"IDAT", zlib.compress(scanlines)) + chunk(b"IEND", b""))


def read_png(path: Path) -> tuple[int, int, bytes]:
    """Decode the small noninterlaced RGBA8 test outputs, verifying every CRC."""
    data = path.read_bytes()
    assert data[:8] == b"\x89PNG\r\n\x1a\n", path
    offset, compressed = 8, bytearray()
    width = height = 0
    while offset < len(data):
        size = struct.unpack_from(">I", data, offset)[0]
        kind, payload = data[offset + 4:offset + 8], data[offset + 8:offset + 8 + size]
        assert zlib.crc32(kind + payload) == struct.unpack_from(">I", data, offset + 8 + size)[0], path
        if kind == b"IHDR":
            width, height, depth, color, compression, filtering, interlace = struct.unpack(">IIBBBBB", payload)
            assert (depth, color, compression, filtering, interlace) == (8, 6, 0, 0, 0), path
        elif kind == b"IDAT":
            compressed.extend(payload)
        offset += size + 12
    raw = zlib.decompress(compressed)
    stride = width * 4
    assert len(raw) == (stride + 1) * height, path
    previous, decoded = bytearray(stride), bytearray()
    for y in range(height):
        start = y * (stride + 1)
        kind, row = raw[start], bytearray(raw[start + 1:start + stride + 1])
        for x in range(stride):
            a, b, c = (row[x - 4] if x >= 4 else 0), previous[x], (previous[x - 4] if x >= 4 else 0)
            if kind == 0:
                prediction = 0
            elif kind == 1:
                prediction = a
            elif kind == 2:
                prediction = b
            elif kind == 3:
                prediction = (a + b) // 2
            elif kind == 4:
                p = a + b - c
                distances = (abs(p - a), abs(p - b), abs(p - c))
                prediction = (a, b, c)[distances.index(min(distances))]
            else:
                raise AssertionError(f"Invalid PNG filter {kind}: {path}")
            row[x] = (row[x] + prediction) & 255
        decoded.extend(row)
        previous = row
    return width, height, bytes(decoded)


PIXELS = [bytes((i * 17 % 256, i * 31 % 256, i * 47 % 256, 0 if i == 6 else 128 if i % 3 == 0 else 255)) for i in range(12)]
RGBA = b"".join(PIXELS)
PNG = png(4, 3, RGBA)


def atlas_plist(texture: str) -> bytes:
    return plistlib.dumps({"frames": {
        "crop.png": {"frame": "{{1,0},{2,2}}", "sourceSize": "{2,2}"},
        "rotated.png": {"frame": "{{0,0},{2,3}}", "sourceSize": "{2,3}", "rotated": True},
        "padded.png": {"frame": "{{0,1},{2,2}}", "sourceSize": "{4,4}", "sourceColorRect": "{{1,1},{2,2}}"},
    }, "metadata": {"realTextureFileName": texture}}, sort_keys=False)


def fixture_assets(game: str) -> tuple[dict[str, bytes], dict[str, bytes]]:
    png_source = wrapped(PNG, "cf4") if game == "cf4" else PNG
    xml = atlas_plist("纹理.png")
    if game in ("cf3", "cf4", "abo"):
        xml = wrapped(xml, game)
    pvr = struct.pack("<13I", 52, 3, 4, 1, 0x12, len(RGBA), 32,
                      0xFF, 0xFF00, 0xFF0000, 0xFF000000, 0x21525650, 1) + RGBA
    ccz = b"CCZp" + struct.pack(">HHII", 0, 2, 0, len(pvr)) + zlib.compress(pvr, 1)
    ccz = ccz[:12] + transform(ccz[12:], KEYS[game], 6, False)
    marker = {"cf1": "Music/Items/test.ogg", "cf2": "Themes_cn/test.dat",
              "cf3": "Carrot3/marker.dat", "cf4": "res/marker.dat", "abo": "pandora/marker.dat"}[game]
    assets = {"纹理.png": png_source, "纹理.plist": xml, "pvr.pvr.ccz": ccz,
              "pvr.plist": atlas_plist("pvr.pvr.ccz"), marker: b"marker"}
    extracted = dict(assets)
    extracted["纹理.png"] = PNG
    return assets, extracted


def apk_bytes(assets: dict[str, bytes]) -> bytes:
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for name, content in assets.items():
            archive.writestr("assets/" + name, content)
        archive.writestr("classes.dex", b"must not be extracted")
    return buffer.getvalue()


def run_cli(command: list[str], expected_code: int = 0) -> list[dict]:
    result = subprocess.run(command, text=True, encoding="utf-8", errors="replace",
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=45,
                            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
    assert result.returncode == expected_code, f"Expected exit {expected_code}, got {result.returncode}: {command}\n{result.stdout}"
    return [value for line in result.stdout.splitlines() if line.startswith("{") and isinstance((value := json.loads(line)), dict)]


def assert_clean(parent: Path) -> None:
    assert not [p.name for p in parent.iterdir() if ".unpacking-" in p.name or ".backup-" in p.name], parent


def assert_png_frames(final: Path) -> None:
    folder = final / "Unpacked_PNG" / "纹理"
    assert read_png(folder / "crop.png") == (2, 2, b"".join(PIXELS[i] for i in (1, 2, 5, 6)))
    assert read_png(folder / "rotated.png") == (2, 3, b"".join(PIXELS[i] for i in (2, 6, 1, 5, 0, 4)))
    padded = [bytes(4)] * 16
    for target, source in ((5, 4), (6, 5), (9, 8), (10, 9)):
        padded[target] = PIXELS[source]
    assert read_png(folder / "padded.png") == (4, 4, b"".join(padded))
    assert len(list((final / "Unpacked_PNG").rglob("*.png"))) == 6


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rust", type=Path, default=ROOT / "target/release/cfunpacker-core.exe")
    args = parser.parse_args()
    rust = [str(args.rust.resolve())]
    assert Path(rust[0]).is_file(), f"Build the core first: {rust[0]}"
    with tempfile.TemporaryDirectory(prefix="cfunpacker-integration-") as temporary:
        work = Path(temporary)
        for game in GAMES:
            assets, extracted = fixture_assets(game)
            apk = work / f"{game}-中文.apk"
            apk.write_bytes(apk_bytes(assets))
            assert run_cli(rust + ["detect", str(apk), "--json"])[-1]["game"] == game
            parent = work / game
            command = rust + ["unpack", str(apk), "--game", "auto", "--output", str(parent), "--workers", "2", "--json-lines"]
            result = run_cli(command)[-1]
            assert result["type"] == "result" and result["frames_written"] == 6 and not result["warnings"], result
            final = parent / apk.stem
            for name, content in extracted.items():
                assert (final / name).read_bytes() == content, name
            assert not (final / "classes.dex").exists()
            assert_png_frames(final)
            readme = (final / "README.md").read_text(encoding="utf-8-sig")
            assert "熔萤FluorescentLava" in readme and "AI" in readme
            sentinel = final / "previous.txt"
            sentinel.write_bytes(b"unchanged")
            run_cli(command, 1)
            assert sentinel.read_bytes() == b"unchanged"
            run_cli(command + ["--overwrite"])
            assert not sentinel.exists()
            assert_clean(parent)
            print(f"PASS {game}: detection, encrypted assets, expected crop/rotation/padding pixels, overwrite and cleanup", flush=True)

        for suffix, method in (("apks", zipfile.ZIP_STORED), ("xapk.2", zipfile.ZIP_DEFLATED)):
            assets, _ = fixture_assets("cf3")
            base = {name: data for name, data in assets.items() if name.endswith(".plist") or name.startswith("Carrot3/")}
            split = {name: data for name, data in assets.items() if name not in base}
            bundle = work / ("bundle." + suffix)
            with zipfile.ZipFile(bundle, "w", compression=method) as archive:
                archive.writestr("base.apk", apk_bytes(base))
                archive.writestr("assets.apk", apk_bytes(split))
                archive.writestr("split_config.arm64_v8a.apk", b"ignored configuration split")
            parent = work / suffix
            assert run_cli(rust + ["detect", str(bundle), "--json"])[-1]["game"] == "cf3"
            result = run_cli(rust + ["unpack", str(bundle), "--output", str(parent), "--json-lines"])[-1]
            assert result["frames_written"] == 6 and not result["warnings"], result
            assert_png_frames(parent / "bundle")
            assert_clean(parent)
        print("PASS APKS/XAPK: stored windows, compressed fallback, numeric suffix, cross-split textures", flush=True)

        for game in GAMES:
            source = work / f"standalone-{game}.apk"
            source.write_bytes(apk_bytes({"single3.png": wrapped(PNG, "cf3"), "single4.png": wrapped(PNG, "cf4")}))
            parent = work / (game + "-standalone")
            result = run_cli(rust + ["unpack", str(source), "--game", game, "--output", str(parent), "--json-lines"])[-1]
            assert result["decrypted_standalone_pngs"] == 2 and not result["warnings"], result
            for name in ("single3.png", "single4.png"):
                assert (parent / source.stem / name).read_bytes() == PNG
            assert_clean(parent)
        print("PASS standalone PNG: both wrapper keys under all five game profiles", flush=True)

        unknown = work / "unknown.apk"
        unknown.write_bytes(apk_bytes({"random.dat": b"random"}))
        assert run_cli(rust + ["detect", str(unknown), "--json"])[-1]["game"] is None
        attacks = [
            {"../escape.txt": b"bad"}, {"nested/../../escape.txt": b"bad"},
            {"nested\\..\\escape.txt": b"bad"}, {"C:/escape.txt": b"bad"},
            {"A.txt": b"first", "a.txt": b"second"}, {"CON.txt": b"bad"},
            {"file:stream": b"bad"}, {"README.md": b"bad"},
            {"a<.png": PNG, "a%3C.png": PNG},
        ]
        for index, assets in enumerate(attacks):
            source = work / f"attack-{index}.apk"
            source.write_bytes(apk_bytes(assets))
            parent = work / f"attack-output-{index}"
            final = parent / source.stem
            final.mkdir(parents=True)
            sentinel = final / "sentinel.txt"
            sentinel.write_bytes(b"unchanged")
            run_cli(rust + ["unpack", str(source), "--game", "cf3", "--output", str(parent), "--overwrite"], 1)
            assert sentinel.read_bytes() == b"unchanged" and list(final.iterdir()) == [sentinel]
            assert_clean(parent)
        encoded = work / "encoded.apk"
        encoded_plist = plistlib.loads(atlas_plist("a<.png"))
        encoded_plist["frames"]["crop<.png"] = encoded_plist["frames"].pop("crop.png")
        encoded.write_bytes(apk_bytes({"dir./a<.png": PNG,
                                       "dir./a<.plist": plistlib.dumps(encoded_plist),
                                       'dir /x>"|?*.png': PNG}))
        parent = work / "encoded-output"
        result = run_cli(rust + ["unpack", str(encoded), "--game", "cf3", "--output", str(parent)])[-1]
        assert result["frames_written"] == 3 and not result["warnings"], result
        assert (parent / "encoded/dir%2E/a%3C.png").read_bytes() == PNG
        assert (parent / "encoded/dir%20/x%3E%22%7C%3F%2A.png").read_bytes() == PNG
        frame = parent / "encoded/Unpacked_PNG/dir%2E/a%3C/crop%3C.png"
        assert read_png(frame) == (2, 2, b"".join(PIXELS[i] for i in (1, 2, 5, 6)))
        assert_clean(parent)
        print("PASS unknown detection, unsafe ZIP rejection, encoded Windows names and collision rejection", flush=True)

        source = work / "cancel.apk"
        payload = bytes(1024 * 1024)
        source.write_bytes(apk_bytes({f"data-{index:03}.bin": payload for index in range(128)}))
        parent = work / "cancel-output"
        final = parent / source.stem
        final.mkdir(parents=True)
        sentinel = final / "sentinel.txt"
        sentinel.write_bytes(b"unchanged")
        process = subprocess.Popen(rust + ["unpack", str(source), "--game", "cf3", "--output", str(parent),
                                           "--overwrite", "--workers", "1", "--json-lines"],
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                   text=True, encoding="utf-8", creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
        watchdog = threading.Timer(30, process.kill)
        watchdog.start()
        sent, lines = False, []
        try:
            for line in process.stdout:
                lines.append(line)
                event = json.loads(line)
                if event.get("stage") == "Extracting" and not sent:
                    process.stdin.write("cancel\n")
                    process.stdin.flush()
                    sent = True
            code = process.wait(timeout=5)
        finally:
            watchdog.cancel()
            if process.poll() is None:
                process.kill()
                process.wait()
        assert sent and code == 2, f"Cancellation failed: {''.join(lines)}"
        assert sentinel.read_bytes() == b"unchanged" and list(final.iterdir()) == [sentinel]
        assert_clean(parent)
        print("PASS cancellation after extraction starts: exit 2, previous output preserved, staging cleaned", flush=True)
    print("All Rust functional checks passed.")


if __name__ == "__main__":
    main()
