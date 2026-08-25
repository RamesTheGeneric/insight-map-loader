# Vendored OpenVR driver header

`headers/openvr_driver.h` is copied verbatim from Valve's OpenVR SDK
(https://github.com/ValveSoftware/openvr), which is licensed **BSD-3-Clause**.
Only this single header is needed: a SteamVR driver compiles against it and
exports `HmdDriverFactory`; it links nothing from the OpenVR SDK.

It is vendored (rather than fetched at build time) so the Windows build has no
network dependency. To update, replace the file from the same upstream path:
`headers/openvr_driver.h` on the tag you want.
