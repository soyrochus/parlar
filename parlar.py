#!/usr/bin/env python3
import os, sys, asyncio, base64, signal, time
import sounddevice as sd
from openai import AsyncOpenAI
from dotenv import load_dotenv
from rich.console import Console, Group
from rich.panel import Panel
from rich.table import Table
from rich.text import Text
from rich.live import Live
from array import array

# Load environment from .env (if present)
load_dotenv()

# --- Audio config (per API requirements: PCM16, 24kHz, mono) ---
SR = 24000
DTYPE = "int16"
CHUNK_MS = 50
FRAMES = SR * CHUNK_MS // 1000  # 1200 frames ~50 ms

# Optional: choose a voice available to Realtime (e.g., alloy, cedar, marin)
VOICE = os.getenv("REALTIME_VOICE", "alloy")

async def main():
    client = AsyncOpenAI()

    # OpenAI realtime websocket connection
    async with client.beta.realtime.connect(model="gpt-realtime") as conn:
        # Shared UI state
        state = {
            "mic_level": 0.0,
            "spk_level": 0.0,
            "mic_bytes": 0,
            "spk_bytes": 0,
            "last_you": "",
            "last_assistant": "",
            "running": True,
        }

        # Resolve devices for display
        try:
            in_dev_idx, out_dev_idx = sd.default.device
        except Exception:
            in_dev_idx, out_dev_idx = None, None
        try:
            in_dev = sd.query_devices(in_dev_idx, kind="input") if in_dev_idx is not None else sd.query_devices(None, kind="input")
        except Exception:
            in_dev = {"name": "(unknown)", "index": in_dev_idx}
        try:
            out_dev = sd.query_devices(out_dev_idx, kind="output") if out_dev_idx is not None else sd.query_devices(None, kind="output")
        except Exception:
            out_dev = {"name": "(unknown)", "index": out_dev_idx}

        # Configure the session: audio I/O, voice, VAD, and instructions
        await conn.session.update(session={
            "modalities": ["audio", "text"],
            "voice": VOICE,
            "instructions": "You are a concise, helpful assistant.",
            "input_audio_format": "pcm16",
            "output_audio_format": "pcm16",
            "turn_detection": {  # server-side VAD
                "type": "server_vad",
                "threshold": 0.5,
                "silence_duration_ms": 400,
                "prefix_padding_ms": 200
            },
        })

        # Queue for mic → network audio chunks
        q: asyncio.Queue[bytes] = asyncio.Queue(maxsize=24)
        stop = asyncio.Event()

        # Mic callback (runs in PortAudio thread)
        def on_mic(indata, frames, time, status):
            if status:
                # Non-fatal; drop if queue is full to avoid blocking
                print(f"[mic] {status}", flush=True)
            try:
                q.put_nowait(bytes(indata))  # bytes in PCM16 little-endian
            except asyncio.QueueFull:
                pass

        # Simple amplitude calculation for PCM16 chunk
        def chunk_peak_level(pcm16_bytes: bytes) -> float:
            if not pcm16_bytes:
                return 0.0
            a = array('h')
            a.frombytes(pcm16_bytes)
            if sys.byteorder != 'little':
                a.byteswap()
            if not a:
                return 0.0
            peak = max(abs(s) for s in a)
            return min(1.0, peak / 32767.0)

        # Stream mic audio → Realtime API (append chunks)
        async def mic_sender():
            with sd.RawInputStream(samplerate=SR, channels=1,
                                   dtype=DTYPE, blocksize=FRAMES,
                                   callback=on_mic):
                while not stop.is_set():
                    chunk = await q.get()
                    # Update UI state based on mic chunk
                    lvl = chunk_peak_level(chunk)
                    state["mic_level"] = lvl
                    state["mic_bytes"] += len(chunk)
                    b64 = base64.b64encode(chunk).decode("ascii")
                    # Send to server-side input buffer
                    await conn.input_audio_buffer.append(audio=b64)

        # Play model audio as it streams
        async def speaker():
            with sd.RawOutputStream(samplerate=SR, channels=1,
                                    dtype=DTYPE, blocksize=FRAMES) as out:
                async for event in conn:
                    t = getattr(event, "type", None)
                    if t == "input_audio_buffer.speech_stopped" or t == "input_audio_buffer.committed":
                        # Ask the model to respond for this turn
                        await conn.response.create()
                    elif t == "response.audio.delta":
                        # Base64 → PCM16 bytes → speaker
                        audio_bytes = base64.b64decode(event.delta)
                        out.write(audio_bytes)
                        # Update speaker level
                        state["spk_level"] = chunk_peak_level(audio_bytes)
                        state["spk_bytes"] += len(audio_bytes)
                    elif t == "response.audio.done":
                        # End of one spoken response
                        pass
                    elif t == "response.text.delta":
                        # Optional: print partial transcript of assistant
                        state["last_assistant"] += event.delta
                        print(event.delta, end="", flush=True)
                    elif t == "response.text.done":
                        print()
                    elif t == "conversation.item.input_audio_transcription.completed":
                        # Optional: print what *you* just said (server transcription)
                        state["last_you"] = event.transcript
                        print(f"\nYOU: {event.transcript}")
                    elif t == "error":
                        print(f"\n[realtime error] {event.error}")
                    # Ignore other events for brevity

        # UI rendering using Rich
        console = Console()

        def bar(level: float, width: int = 24) -> str:
            level = 0.0 if level is None else max(0.0, min(1.0, float(level)))
            filled = int(round(level * width))
            return f"[{'█'*filled}{' '* (width-filled)}] {int(level*100):3d}%"

        def render_ui():
            dev_tbl = Table(show_header=True, header_style="bold cyan")
            dev_tbl.add_column("Device", justify="right", style="bold")
            dev_tbl.add_column("Name", overflow="fold")
            dev_tbl.add_column("Index", justify="right")
            dev_tbl.add_row("Mic", str(in_dev.get('name', '(unknown)')), str(in_dev_idx) if in_dev_idx is not None else "-")
            dev_tbl.add_row("Speaker", str(out_dev.get('name', '(unknown)')), str(out_dev_idx) if out_dev_idx is not None else "-")

            meters_tbl = Table.grid(expand=True)
            meters_tbl.add_row(Text("Mic", style="bold"), Text(bar(state["mic_level"])) , Text(f"{state['mic_bytes']//1024} KiB", style="dim"))
            meters_tbl.add_row(Text("Spk", style="bold"), Text(bar(state["spk_level"])), Text(f"{state['spk_bytes']//1024} KiB", style="dim"))

            info_tbl = Table.grid(expand=True)
            info_tbl.add_row(Text("You:", style="bold green"), Text(state["last_you"][-80:]))
            info_tbl.add_row(Text("AI:", style="bold magenta"), Text(state["last_assistant"][-80:]))

            cmds = Text("Commands: [Q] Quit", style="yellow")

            group = Group(dev_tbl, meters_tbl, info_tbl, cmds)
            return Panel(group, title="Parlar Realtime", subtitle=f"SR={SR}Hz DTYPE={DTYPE} chunk={CHUNK_MS}ms", border_style="blue")

        async def ui_task():
            with Live(render_ui(), console=console, refresh_per_second=10) as live:
                while not stop.is_set():
                    await asyncio.sleep(0.1)
                    live.update(render_ui())

        async def keyboard_task():
            # Read lines; 'q' or 'Q' then Enter to quit
            loop = asyncio.get_running_loop()
            while not stop.is_set():
                line = await loop.run_in_executor(None, sys.stdin.readline)
                if not line:
                    continue
                if line.strip().lower() == 'q':
                    stop.set()
                    break

        # Graceful shutdown on Ctrl+C
        loop = asyncio.get_running_loop()
        loop.add_signal_handler(signal.SIGINT, stop.set)

        await asyncio.gather(mic_sender(), speaker(), ui_task(), keyboard_task())

if __name__ == "__main__":
    # Ensure API key is set (supports OPENAI_AI_KEY for convenience)
    api_key = os.getenv("OPENAI_API_KEY") or os.getenv("OPENAI_AI_KEY")
    if api_key:
        os.environ["OPENAI_API_KEY"] = api_key
    if not os.getenv("OPENAI_API_KEY"):
        raise SystemExit("Set OPENAI_API_KEY or OPENAI_AI_KEY in your .env or environment")
    asyncio.run(main())
