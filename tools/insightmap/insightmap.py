"""insightmap -- talk to a puck's persisted Insight map over adb.

A thin `Device` wrapper: query the live map context and pull
`/vision/insideout/mapdb` to the host. Decoding is `mapdata.py`; matching and
alignment are `matchmap.py`.

The map is read from the FILES, not from process memory. Extracting it from the
heap is possible and was tried; it recovers structure that is genuinely spatial
but an order of magnitude short of the file's precision, and it needs root plus
a permissive SELinux window. See FINDINGS.md.

    d = Device("192.168.1.10")
    if d.mapdb_files():
        local = d.pull_mapdb("/tmp/mapdb")     # -> decode with mapdata.Map()
"""
import os
import re
import subprocess


class Device:
    """adb wrapper for one Quest 1 (wifi adb serial `<ip>:5555`)."""

    def __init__(self, ip: str, port: int = 5555):
        self.serial = ip if ":" in ip else f"{ip}:{port}"

    def sh(self, cmd: str, timeout: int = 30) -> str:
        r = subprocess.run(["adb", "-s", self.serial, "shell", cmd],
                           capture_output=True, timeout=timeout)
        return r.stdout.decode(errors="replace")

    def push(self, local: str, remote: str, timeout: int = 120):
        subprocess.run(["adb", "-s", self.serial, "push", local, remote],
                       capture_output=True, timeout=timeout)

    def pull(self, remote: str, local: str, timeout: int = 180):
        subprocess.run(["adb", "-s", self.serial, "pull", remote, local],
                       capture_output=True, timeout=timeout)

    def map_info(self) -> dict:
        d = self.sh("dumpsys tracking 2>/dev/null", timeout=25)
        info: dict = {"raw_len": len(d)}
        m = re.search(r"MapPoint (\d+).*?Descriptors (\d+)", d)
        if m:
            info["map_points"] = int(m.group(1))
            info["descriptors"] = int(m.group(2))
        m = re.search(r"topNodeUid ([0-9a-f-]+)", d)
        if m:
            info["map_uuid"] = m.group(1)
        info["l1_nodes"] = re.findall(r"L1 node ([0-9a-f]+) hosts", d)
        m = re.search(r"anchor ([0-9a-f-]+) ->.*?worldOrigin (\w)", d)
        if m:
            info["anchor_uuid"], info["anchor_world_origin"] = m.group(1), m.group(2)
        return info

    # ---- the persisted map: strictly better than any memory dump when present
    def mapdb_files(self) -> list[str]:
        out = self.sh("ls /vision/insideout/mapdb 2>/dev/null", timeout=20)
        return [f for f in out.split() if f.endswith(".mapdata")]

    def pull_mapdb(self, out_dir: str) -> str:
        """Pull /vision/insideout/mapdb and return the local directory.

        When trackingservice has persisted the map, this replaces the whole
        forensics path: exact points, their descriptors already paired, and the
        node poses to place them in one frame -- no root, no permissive, no
        process_vm_readv. Decode with mapdata.Map(). Empty until the map
        actually persists (see docs/insight-mapdata-format.md)."""
        os.makedirs(out_dir, exist_ok=True)
        self.pull("/vision/insideout/mapdb", out_dir, timeout=180)
        inner = os.path.join(out_dir, "mapdb")
        return inner if os.path.isdir(inner) else out_dir
