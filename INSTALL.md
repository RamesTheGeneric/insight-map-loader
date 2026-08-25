# Installing Insight Prime

Three things get built: the **app** that runs on each puck, the **host service**
that aggregates them, and the **SteamVR driver** that presents them as trackers.
Then each headset is brought up individually — see
[docs/puck-bringup.md](docs/puck-bringup.md).

> Verified on Linux with two rooted Quest 1s. The driver also builds on Windows,
> which is where it must be built if SteamVR runs there. Nothing here has been
> reproduced on another person's hardware yet.

## Prerequisites

| For | Need |
|---|---|
| the puck app | JDK 17, Android SDK, Android NDK, CMake 3.22.1 (SDK Manager installs both) |
| the host service | Rust ≥ 1.75 (`rustup`) |
| the SteamVR driver | CMake ≥ 3.15 and a C++17 compiler |
| the map tools | Python 3 with `numpy` |
| everything | `adb` (`android-tools`), and **rooted Quest 1 headsets** |

Rooting a Quest 1 is out of scope. Without root, nothing below works.

## 1. The puck app

```sh
cd android/q1tracker
./gradlew :app:assembleDebug
# -> app/build/outputs/apk/debug/app-debug.apk
```

Keep that path; `q1bringup.sh --apk` wants it. If Gradle cannot find the SDK,
create `android/q1tracker/local.properties` with `sdk.dir=/path/to/Android/Sdk`.

## 2. The host service

```sh
cargo build --release --manifest-path desktop/Cargo.toml
```

Produces `desktop/target/release/insight-prime` (CLI) and `insight-prime-gui`.

Then create your site config:

```sh
cp desktop/insight-prime.example.json insight-prime.json
```

Edit `host` to **your PC's LAN IP** — the pucks stream to it, so `127.0.0.1`
will not do. Puck entries can be left for `q1bringup.sh` to add.

## 3. The SteamVR driver

```sh
cd desktop/steamvr_driver
cmake -S . -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build -j
```

Register it with SteamVR (once). This points SteamVR at the build tree, so
rebuilding *is* reinstalling:

```sh
~/.steam/steam/steamapps/common/SteamVR/bin/linux64/vrpathreg \
    adddriver "$(pwd)/build/mapper"
```

Verify with no headsets involved — start SteamVR, then:

```sh
python3 desktop/steamvr_driver/tools/send_test_tracker.py --device 6
```

Exactly one tracker should appear, as Left Elbow, and **nothing else**. If a
dozen appear, the driver did not load the build you just made.

## 4. Bring up each puck

```sh
./tools/q1bringup.sh <ip> --usb <SERIAL> --apk android/q1tracker/app/build/outputs/apk/debug/app-debug.apk --device 0
```

It verifies each step rather than assuming, and ends by listing what is left.
The reasoning behind each step is in [docs/puck-bringup.md](docs/puck-bringup.md).

## 5. Run it

```sh
./run-gui.sh
```

From the GUI: **⟳ Launch trackers**, then give the fleet a shared map
(**⇄ Share map**, or **✚ Create map** on the first puck in a new space), then
hold the pucks still and press **⌖ Bridge now**.

The fleet banner turns green when every puck is on the same map. Confirm
physically by holding two pucks together and running `tools/q1sep.py` — expect
a few centimetres.

## Troubleshooting

The failure modes here are mostly *silent* — a puck that looks healthy in every
indicator and emits nothing. The symptom-keyed table in
[docs/puck-bringup.md](docs/puck-bringup.md#troubleshooting) is the fastest way
in; the two that catch everyone are **the guardian package must be disabled
before the tracker app starts**, and **adb root does not survive a reboot**.
