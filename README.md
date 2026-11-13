# Cable Mixer

Voicemeeter, but easy. Mix **one microphone** and **one program's audio** into
[VB-Cable](https://vb-audio.com/Cable/) so other apps can use the mix as a microphone.

- Pick a mic and a program, each with its own volume (0-200%) and mute
- Captures only the chosen program's audio (Windows per-process loopback)
- Remembers your last choices and re-attaches to the program when it starts
- Low latency (roughly 20-40 ms) using event-driven WASAPI

## Usage

1. Install [VB-Cable](https://vb-audio.com/Cable/) (reboot afterwards).
2. Download `cable-mixer.zip` from the [Releases](../../releases) page and run `cable-mixer.exe`.
3. Choose your microphone and the program whose audio you want to share.
4. In Discord, OBS, a game, etc., set the microphone to **CABLE Output**.

Settings are stored in `%APPDATA%\CableMixer\settings.json`.

## Requirements

- Windows 11 or a recent Windows 10 (per-app capture needs it)
- VB-Cable installed

## Build from source

```
cargo build --release
```

Needs Rust 1.95 or newer. The binary is `target\release\cable-mixer.exe`.

## Notes

- Only programs that have played audio appear in the program list.
- The mix is hard-clipped at full scale; lower the sliders if it distorts.
- VB-Cable is made by VB-Audio and is not included or affiliated with this project.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
