# Contributing

Thanks for looking. This is a small project with an unusual constraint: **it has
only ever run on two rooted Quest 1 headsets, on one network, in one room.**
Everything in the results table is reproducible here and none of it is
reproducible anywhere else until someone else tries. That shapes what is useful
to contribute.

For toolchains, the build-and-iterate loop, and how to work on this without
hardware, see **[docs/development.md](docs/development.md)**.

## The most valuable contribution

**Tell us it worked, or didn't, on your hardware.** A report from a third
headset, a different room, a different Quest 1 build number, or a Windows host is
worth more right now than a feature. The honest limits in the README exist
because nobody has checked; each one someone checks is a real result.

A useful report includes:

- headset model and build number (`getprop ro.build.version.incremental`)
- what `insight-map-loader status` printed for each puck
- the tracking level and map context line from `dumpsys tracking`
- if pucks disagreed in space, the output of `tools/q1sep.py`

⚠️ **Never attach a `mapdb` or `.mapdata` file.** A persisted Insight map is a
3-D point cloud of the room it was made in — someone's home. Describe it, or
send a screenshot of `visualize3d.py`, but do not send the map.

Also scrub device serials, WiFi MACs and your `insight-map-loader.json` out of
anything you paste.

## What fits here

**Welcome:**

- reports and fixes from other hardware, other rooms, other hosts
- making silent failures loud — this project's characteristic bug is a component
  that looks healthy and does nothing
- the unbuilt refinement in
  [docs/insight-slam-internals.md](docs/insight-slam-internals.md): continuous
  exchange of *compact submaps* between pucks instead of one founding map, which
  is how Meta aligns self-tracked controllers
- decoder and matcher work in `tools/insightmap/`, which is testable offline
- documentation corrections, especially where a doc claims something the code no
  longer does

**Probably not:**

- **hand-applied offsets or manual calibration steps.** If two pucks disagree,
  the fix is to find out why, not to add a correction the user has to tune. The
  project deleted a whole alignment subsystem for this reason.
- support for headsets nobody in the discussion owns
- reformatting sweeps (see below)
- rooting instructions — deliberately out of scope

If you are planning something large, open an issue first. Not for permission —
so you find out whether it was already tried and failed, which for a lot of the
obvious ideas here it was. [FINDINGS.md](FINDINGS.md) is the record of that and
is worth reading before you start.

## The bar for a change

**Measure it, don't assert it.** This project has retracted claims before —
memory-extraction results that beat a badly-chosen control, a "sparsity floor"
that was an artefact. The habits that came out of that:

- **Use a matched control.** An unmatched one made a meaningless cluster score
  perfectly. If you are claiming a result is better than chance, make chance
  work as hard as your result does.
- **State what your check cannot see.** Internal-consistency checks are cheap
  and mislead in both directions; external ground truth is the one that counts.
  `tools/q1sep.py` is the standard here — two co-located pucks cannot lie to
  each other.
- **A refusal beats a confident wrong answer.** `align_stable()` returns `None`
  when its RANSAC seeds disagree, because a wrong transform is worse than none.
  Prefer that shape.

Concretely, before sending a change:

```sh
cargo test --manifest-path desktop/Cargo.toml     # 26 tests, must stay green
```

Add a test if the thing you changed can be tested without a headset — wire
format, transform maths, config handling, job ordering and drift logic all can.
Name it as a sentence describing the behaviour (`a_puck_without_a_bridge_is_omitted`),
because the name is what someone reads when it breaks.

If you changed something that only a headset can exercise, say so plainly in the
PR and say what you ran it against. "Untested on hardware" is a fine thing to
write; a claim that it works when you didn't check is not.

## Conventions

**Comments explain *why*, and record what was learned.** The codebase is
unusually heavy on this and it is intentional — most of the tricky code here
exists because of a specific discovered behaviour of an undocumented system, and
without the reason the code reads as arbitrary and gets "simplified" back into a
bug. If you fix something subtle, leave the finding behind:

```rust
// Never pipe `dumpsys tracking` into a head/grep that closes the pipe
// early — that leaves the tracking service unavailable for many seconds
// ("Can't find service: tracking"). Dump to a file on-device, grep the file.
```

**Do not run `cargo fmt` across the tree.** It is not currently
rustfmt-formatted, so a sweep produces a thousand-line diff that buries your
change and redirects every future `git blame` at the reformat. Match the style
around you. A deliberate whole-tree reformat is fine as its own commit that
changes nothing else.

**Commits**: imperative subject, and a body that explains why rather than
restating the diff. Author your own commits under your own name.

**Never break these**, they are load-bearing across components:

- **MPT1 role ids are APPEND ONLY** — never renumber, never rename. SteamVR keys
  pairings and room calibration off them, so renumbering silently rebinds a
  user's trackers to the wrong body parts.
- **The 68-byte packet layout is stated in three files that must agree** —
  `mpt1.rs`, `mapper_protocol.h`, `send_test_tracker.py`.
- **The two adb rules** in
  [docs/development.md](docs/development.md#rules-for-anything-that-touches-a-puck).

## Things that must never be committed

`.gitignore` covers these, but it has failed before, so check what you are
staging:

- `*.mapdata` / any `mapdb` directory — a 3-D scan of a home
- device serials, WiFi MACs, your LAN addresses
- `insight-map-loader.json`, `bridge.json` — site-specific, and they name your
  network
- APKs, keystores, build output
- **any file pulled off a Quest** — the device libraries are Meta's, and this
  project ships no Meta code at all. It interoperates with software already on a
  device you own, using interfaces recovered by observation. Keep it that way.

## Licence

GPL-3.0. By contributing you agree your contribution ships under it. See
[LICENSE](LICENSE), and [THIRD_PARTY.md](THIRD_PARTY.md) for the vendored
`openvr_driver.h` (BSD-3).
