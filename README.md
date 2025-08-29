Parlar — OpenAI Realtime Demo. Chat freely with an AI
================================

Parlar is a small terminal demo for the OpenAI Realtime API. It captures your microphone, streams audio to the model, and plays the model’s audio replies with low latency. It supports barge‑in (you can start talking to interrupt the model), a minimal live UI, and quick keyboard control.

Features
- Realtime audio in/out with low latency
- Barge‑in: talk to cancel an ongoing reply
- Live terminal UI with mic/speaker meters
- Simple setup via `uv` and `.env`

Requirements
- Python `>=3.13`
- macOS, Linux, or Windows with working audio input/output
- An OpenAI API key with Realtime access (`OPENAI_API_KEY`)

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

Project Layout
- `parlar.py`: main script with realtime client, audio I/O, barge‑in, and UI
- `pyproject.toml`: project metadata and dependencies
- `uv.lock`: pinned dependency versions for reproducible installs

License
MIT License

Copyright (c) 2025 Iwan van der Kleijn

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the “Software”), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED “AS IS”, WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
