Parlar — OpenAI Realtime Demo. Chat freely with an AI
================================

Parlar is a small terminal demo for the OpenAI Realtime API. It captures your microphone, streams audio to the model, and plays the model’s audio replies with low latency. It supports barge‑in (you can start talking to interrupt the model), a minimal live UI, and quick keyboard control.

Features
- Realtime audio in/out with low latency
- Barge‑in: talk to cancel an ongoing reply
- Live terminal UI with mic/speaker meters
- Simple setup via `uv` and `.env`

Requirements
- macOS, Linux, or Windows with working audio input/output
- An OpenAI API key with Realtime access (`OPENAI_API_KEY`)
- Python `>=3.13` (for the Python app)
- Rust toolchain (`cargo`) if using the Rust app

Install (via uv)
1) Install `uv` (if you don’t have it):
   - macOS/Linux: `curl -LsSf https://astral.sh/uv/install.sh | sh`
   - Homebrew: `brew install uv`
   - Windows (PowerShell): `iwr https://astral.sh/uv/install.ps1 -UseB -OutFile install.ps1; ./install.ps1`

2) Sync dependencies in this project (uses the included `uv.lock`):
   - `uv sync`

3) Ensure audio permissions are granted to your terminal (OS prompt on first run).

Configuration
- Create a `.env` file in the project root with at least:
  - `OPENAI_API_KEY=sk-...`  Your API key.
  - Optional: `REALTIME_VOICE=alloy`  Voice id for TTS output.
  - Optional tuning knobs:
    - `BAR_GE_THRESH=0.20` Peak gate for barge‑in (0–1)
    - `CANCEL_COOLDOWN_MS=400` Minimum ms between cancels
    - `SUPPRESS_AFTER_CANCEL_MS=800` Drop late deltas window

Usage
- Run from the project root with `uv`:
  - `uv run ./parlar.py`
- Controls:
  - Press `q` to quit (no Enter needed)
  - Speak to talk; if the model is speaking, speaking louder than the threshold will cancel it (barge‑in).

Examples
- Minimal run (reads `OPENAI_API_KEY` from `.env`):
  - `uv run ./parlar.py`

- One‑off voice selection:
  - `REALTIME_VOICE=verse uv run ./parlar.py`

- Tighter barge‑in (more sensitive):
  - `BAR_GE_THRESH=0.15 uv run ./parlar.py`

- Use an API key just for this session:
  - `OPENAI_API_KEY=sk-... uv run ./parlar.py`

Notes
- Parlar uses `sounddevice` for audio and `rich` for the terminal UI.
- If you see audio backend errors on Linux, install PortAudio runtime/dev packages (e.g., `sudo apt-get install -y libportaudio2`), then re‑try.
- The app looks for `OPENAI_API_KEY` (or `OPENAI_AI_KEY`) and will exit with an error if not found.
- The Realtime session is configured for audio + text modalities and PCM16 audio at 24 kHz.

Rust App
--------

The Rust CLI in `src/main.rs` implements the same realtime flow with adaptive turn‑taking and robust, interruptible TTS.

Build & Run
- Build: `cargo build`
- Run: `cargo run -release` (loads `.env` automatically)

Controls
- `I`: Interrupt the assistant mid‑reply (cancel + truncate)
- `Q`: Quit

Environment Options (Rust)
- `OPENAI_API_KEY`: API key (required)
- `REALTIME_MODEL`: Realtime model id (default `gpt-realtime`)
- `REALTIME_VOICE`: TTS voice id (default `alloy`)
- `SR`: Sample rate Hz (default `24000`)
- `CHUNK_MS`: Mic chunk size ms (default `20`)
- `HALF_DUPLEX`: Suppress mic while assistant speaks (default `true`)
- `BAR_GE_THRESH`: Energy threshold for barge‑in, 0–1 (default `0.20`)
- `CANCEL_COOLDOWN_MS`: Minimum ms between cancels (default `400`)
- `SUPPRESS_AFTER_CANCEL_MS`: Drop late deltas window ms (default `800`)
- `TURN_SIL_MS`: Server VAD silence ms before commit (default `350`)
- `TURN_VAD_THRESH`: Server VAD energy threshold (default `0.55`)
- `RESP_DELAY_SHORT_MS`: Extra delay after clear sentence end (default `200`)
- `RESP_DELAY_LONG_MS`: Extra delay after ambiguous end (default `700`)

Behavior Highlights (Rust)
- Continuous streaming mic input with incremental transcription.
- Adaptive turn‑taking: responds only after end‑of‑turn commit plus short, context‑aware delay.
- Interruptible speech: energy‑based and keyword‑based (e.g., “stop”, “hey”, “wait”).
- Noise‑robust: requires sustained onset for barge‑in, cooldowns/suppression avoid false triggers.

Troubleshooting Barge‑in (Rust)
- If speaking doesn’t interrupt: lower `BAR_GE_THRESH` (e.g., `0.12`) or disable half‑duplex by setting `HALF_DUPLEX=false` to allow full‑duplex mic while the assistant speaks.
- In half‑duplex, the app now “leaks” only clearly loud onsets to the server so keyword‑based interrupts (“stop”, “wait”, “hey”) still work. If your mic is quiet, reduce `BAR_GE_THRESH`.
- Ensure input gain is reasonable and that your default input/output devices are correct.

Rust Audio Notes
- Uses `cpal` for cross‑platform audio I/O and `crossterm` for non‑blocking keys.
- On Linux, ensure ALSA is available; on some systems you may need: `sudo apt-get install -y libasound2 libasound2-dev`.

Project Layout
- `parlar.py`: main Python realtime client, audio I/O, barge‑in, and UI
- `src/main.rs`: Rust realtime client (audio I/O, adaptive turn‑taking, barge‑in)
- `Cargo.toml`: Rust crate manifest
- `pyproject.toml`: Python project metadata and dependencies
- `uv.lock`: pinned dependency versions for reproducible installs

## Contributing

Pull requests are welcome. For major changes, please open an issue first to discuss what you would like to change.

Please make sure to update tests as appropriate.


## License and Copyright

MIT License. See [LICENSE](LICENSE.txt) for details.

Copyright © 2025 Iwan van der Kleijn
