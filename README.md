# horizon-auto-fishing

<p align="center"><img src="logo.png" alt="horizon-auto-fishing logo: a fishing bobber on the water" width="180"></p>

<p align="center">
  <a href="https://github.com/SkyeSlav/horizon-auto-fishing/releases/latest"><img src="https://img.shields.io/github/v/release/SkyeSlav/horizon-auto-fishing" alt="Latest release"></a>
  <a href="https://github.com/SkyeSlav/horizon-auto-fishing/actions/workflows/release.yml"><img src="https://github.com/SkyeSlav/horizon-auto-fishing/actions/workflows/release.yml/badge.svg" alt="Release build status"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/SkyeSlav/horizon-auto-fishing" alt="License: MIT"></a>
</p>

Auto fishing for Horizon/Starling: you cast first, and the mod reels in on the bite and casts again. Idea inspired by Simple Auto Fishing, Auto Fishing + and Auto Fisher Pro — a clean-room Rust reimplementation, no code copied.

> **Releases:** grab the ready-built file from the [latest release](https://github.com/SkyeSlav/horizon-auto-fishing/releases/latest) — no build needed. `auto-fishing-1.0.0.hmod` is for Starling 0.9.0 and the Horizon launcher. The `.hnmod` (Starling 0.8.0) build was discontinued.

- **You cast first** — the mod never casts on its own. Cast by hand and it takes over: reel in on the bite, then cast again after the recast delay (default 17 ticks). Reel in by hand and it stops until you cast again.
- **Human-like delays** — every delay gets a small random extra (up to 5 ticks by default), so no two fishing cycles are exactly alike.
- **Break protection** — a rod is not used once 5 or fewer durability remain.
- **Rod swapping** — in the all-rods modes, a worn-out rod is swapped for the freshest rod in your hotbar or inventory.
- **Bobber-killed recast** — if the server kills the bobber, the rod is cast again after the delay.
- **Junk filter** — off by default, with an empty list. It only throws away items you list and turn on.

Works on servers. Some servers treat auto-fishing as cheating — their rules apply.

## Install

### Horizon launcher (`.hmod`, 1.0.0)

Put `auto-fishing-1.0.0.hmod` (the release file) in the instance's
`horizon-mods/` folder and add `mod.auto_fishing:true` to
`horizon-mods.txt` in the same game folder — or run
`horizon-launcher native-build hmod --instance "<instance>" --trust-native-code`.

On Starling 0.9.0, import the `.hmod` from the Content Library.

Built for Starling 0.9.0. It hears the bite's splash sound instead of
watching the bobber, since the launcher mod API has no bobber read.

Build it from this repo if you want (needs Rust 1.99+ and the
`cargo-horizon-mod` tool; the tool fetches the pinned SDK itself):

```sh
cargo install --locked --git https://github.com/horizon-rust/horizon cargo-horizon-mod
cd hmod && cargo horizon-mod build   # target/horizon-mod/auto_fishing.hmod
```

## Commands

- `/autofish` — status.
- `/autofish toggle` — turn auto fishing on/off.
- `/autofish mode <1-4>` — 1 current rod, 2 + break protection, 3 all rods, 4 + break protection.
- `/autofish delay <ticks>` — wait before recasting after a catch (0–100, default 17).
- `/autofish catch <ticks>` — wait after the bite before reeling in (0–20, default 0).
- `/autofish jitter <ticks>` — random extra delay, up to N ticks (0–20, default 5).
- `/autofish jitter on/off` — human-like delays on/off.
- `/autofish junk on/off` — throw away listed junk (default off).
- `/autofish junk add <item> / remove <item> / list / clear / preset` — manage the junk list.
- `/autofish help` — lists the commands.

The settings page (Options > Mods > Auto Fishing) has the toggle, the mode, the two delays, the human-like delays and the junk filter.

## How it works

- A fishing session starts only when you cast by hand. From then on the mod reels in on the bite, and casts again after the delay, until you reel in by hand or turn the mod off.
- If the rod you are holding is about to break, the mod swaps in the freshest usable rod it can find in your hotbar or inventory — in the all-rods modes — and never casts with a rod at 5 or fewer durability in the protected modes.
- The junk filter throws away one listed item per tick, and only while it is on and the list is not empty.

## Layout

- `hmod/`: the 1.0.0 native `.hmod` (Rust crate on the mod API) — the same engine, with the game-facing code in `src/lib.rs`.
- `horizon-registry.toml`: name, summary, description, authors and license for the registry entry.

## License

MIT, see [LICENSE](LICENSE).
