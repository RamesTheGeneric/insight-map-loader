# Third-party components

Insight Prime is GPL-3.0. The components below are included or depended upon
under their own terms.

## Vendored in this repository

| Component | Where | Licence |
|---|---|---|
| `openvr_driver.h` | `desktop/steamvr_driver/vendor/openvr/headers/` | BSD-3-Clause, © Valve Corporation |
| Gradle wrapper | `android/q1tracker/gradle/` | Apache-2.0 |

`openvr_driver.h` is the only OpenVR file present; the SDK is not required to
build the driver.

## Fetched at build time

| Component | Used by | Licence |
|---|---|---|
| OpenXR loader | `android/q1tracker` (Maven) | Apache-2.0 |
| `eframe` / `egui` | `desktop/insight-prime-gui` | MIT / Apache-2.0 |
| `serde`, `serde_json` | `desktop/insight-prime-core` | MIT / Apache-2.0 |
| numpy | `tools/insightmap` | BSD-3-Clause |

Full Rust dependency licences: `cargo tree --format '{p} {l}'` in `desktop/`.

## NOT included, and why

Building against a Quest's HAL needs link stubs from the device's own system
libraries. Those are **Meta's copyrighted binaries** and are not distributed
here under any circumstances. Where a build needs them it reads them from a
headset you own, over adb, at build time.

Likewise, this repository contains no extracted Meta application code, no
firmware, and no boot images.

## On interoperability

The wire formats, file formats and service interfaces this project speaks to
were recovered by observing a device's behaviour and its own diagnostic output.
The findings are documented in `docs/` and `FINDINGS.md` as descriptions of an
interface, not as copied implementation.
