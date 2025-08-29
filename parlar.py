#!/usr/bin/env python3
# Parlar Realtime (hardened): barge-in, fast endpointing, reliable 'q'
import os, sys, asyncio, base64, signal, time, threading
from array import array

import sounddevice as sd
from dotenv import load_dotenv
from openai import AsyncOpenAI
from rich.console import Console, Group
from rich.panel import Panel
from rich.table import Table
from rich.text import Text
from rich.live import Live

load_dotenv()

# ---- Audio config (PCM16 mono @ 24 kHz) ----
SR = 24000
DTYPE = "int16"
CHUNK_MS = 20                       # small chunks reduce latency
FRAMES = SR * CHUNK_MS // 1000

VOICE = os.getenv("REALTIME_VOICE", "alloy")

# ---- Barge-in tuning ----
BAR_GE_THRESH = float(os.getenv("BAR_GE_THRESH", "0.20"))  # 0..1 peak
CANCEL_COOLDOWN_MS = int(os.getenv("CANCEL_COOLDOWN_MS", "400"))
SUPPRESS_AFTER_CANCEL_MS = int(os.getenv("SUPPRESS_AFTER_CANCEL_MS", "800"))

def chunk_peak_level(pcm16_bytes: bytes) -> float:
    if not pcm16_bytes:
        return 0.0
    a = array('h'); a.frombytes(pcm16_bytes)
    if sys.byteorder != 'little':
        a.byteswap()
    if not a:
        return 0.0
    peak = max(abs(s) for s in a)
    return min(1.0, peak / 32767.0)

async def main():
    # Device info (best-effort, for UI only)
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

    client = AsyncOpenAI()
    console = Console()
    stop = asyncio.Event()

    # Shared state
    state = {
        "mic_level": 0.0,
        "spk_level": 0.0,
        "mic_bytes": 0,
        "spk_bytes": 0,
        "last_you": "",
        "last_assistant": "",
        # response lifecycle
        "response_active": False,          # True between first delta and audio.done/text.done
        "last_cancel_ts": 0.0,             # ms epoch
        "cancel_guard_until": 0.0,         # ms epoch to drop stale deltas
        "response_inflight": False,        # True after create() until done/canceled
        "last_assistant_item_id": None,    # track for truncate()
    }

    # ---- UI ----
    def bar(level: float, width: int = 24) -> str:
        level = 0.0 if level is None else max(0.0, min(1.0, float(level)))
        filled = int(round(level * width))
        return f"[{'█'*filled}{' '*(width-filled)}] {int(level*100):3d}%"

    def render_ui():
        dev_tbl = Table(show_header=True, header_style="bold cyan")
        dev_tbl.add_column("Device", justify="right", style="bold")
        dev_tbl.add_column("Name", overflow="fold")
        dev_tbl.add_column("Index", justify="right")
        dev_tbl.add_row("Mic", str(in_dev.get('name', '(unknown)')), str(in_dev_idx) if in_dev_idx is not None else "-")
        dev_tbl.add_row("Speaker", str(out_dev.get('name', '(unknown)')), str(out_dev_idx) if out_dev_idx is not None else "-")

        meters_tbl = Table.grid(expand=True)
        meters_tbl.add_row(Text("Mic", style="bold"), Text(bar(state["mic_level"])), Text(f"{state['mic_bytes']//1024} KiB", style="dim"))
        meters_tbl.add_row(Text("Spk", style="bold"), Text(bar(state["spk_level"])), Text(f"{state['spk_bytes']//1024} KiB", style="dim"))

        info_tbl = Table.grid(expand=True)
        info_tbl.add_row(Text("You:", style="bold green"), Text(state["last_you"][-160:]))
        info_tbl.add_row(Text("AI:", style="bold magenta"), Text(state["last_assistant"][-160:]))

        cmds = Text("Commands: [Q] Quit", style="yellow")
        subtitle = f"SR={SR}Hz DTYPE={DTYPE} chunk={CHUNK_MS}ms | barge≥{BAR_GE_THRESH:.2f}"

        return Panel(Group(dev_tbl, meters_tbl, info_tbl, cmds), title="Parlar Realtime", subtitle=subtitle, border_style="blue")

    async def ui_task():
        with Live(render_ui(), console=console, refresh_per_second=15) as live:
            while not stop.is_set():
                await asyncio.sleep(0.066)
                live.update(render_ui())

    # ---- Realtime + audio plumbing ----
    async with client.beta.realtime.connect(model="gpt-realtime") as conn:
        # Event loop reference (used by background key thread)
        loop = asyncio.get_running_loop()
        await conn.session.update(session={
            "modalities": ["audio", "text"],
            "voice": VOICE,
            "instructions": "You are a concise, helpful assistant.",
            "input_audio_format": "pcm16",
            "output_audio_format": "pcm16",
            # Enable user input transcripts:
            "input_audio_transcription": { "model": "whisper-1" },
            # Keep VAD but avoid auto-creating responses; we create manually:
            "turn_detection": {
                "type": "server_vad",
                "threshold": 0.5,
                "silence_duration_ms": 150,   # tighter endpointing
                "prefix_padding_ms": 100,
                "create_response": False
            },
        })

        mic_q: asyncio.Queue[bytes] = asyncio.Queue(maxsize=96)
        audio_out_q: asyncio.Queue[bytes] = asyncio.Queue(maxsize=256)

        # Single-key reader → async queue (press 'q' to quit, no Enter)
        key_q: asyncio.Queue[str] = asyncio.Queue()
        _orig_termios = None
        _stdin_fd = None

        if os.name == "nt":
            def key_thread():
                import msvcrt
                while True:
                    if stop.is_set():
                        break
                    if msvcrt.kbhit():
                        try:
                            ch = msvcrt.getwch()
                        except Exception:
                            ch = None
                        if ch and ch.lower() == 'q':
                            try:
                                asyncio.run_coroutine_threadsafe(key_q.put('q'), loop)
                            except Exception:
                                pass
                            break
                    time.sleep(0.03)

            threading.Thread(target=key_thread, daemon=True).start()
        else:
            # POSIX: put stdin into cbreak and poll for single chars
            try:
                import termios, tty, select
                _stdin_fd = sys.stdin.fileno()
                _orig_termios = termios.tcgetattr(_stdin_fd)
                tty.setcbreak(_stdin_fd)
            except Exception:
                _orig_termios = None
                _stdin_fd = None

            def key_thread():
                if _stdin_fd is None:
                    return
                import select, os as _os
                while True:
                    if stop.is_set():
                        break
                    try:
                        r, _, _ = select.select([sys.stdin], [], [], 0.05)
                    except Exception:
                        r = []
                    if r:
                        try:
                            ch = _os.read(_stdin_fd, 1)
                        except Exception:
                            ch = b""
                        if ch and chr(ch[0]).lower() == 'q':
                            try:
                                asyncio.run_coroutine_threadsafe(key_q.put('q'), loop)
                            except Exception:
                                pass
                            break

            threading.Thread(target=key_thread, daemon=True).start()

        def on_mic(indata, frames, time_info, status):
            if status:
                # avoid chatty logs
                pass
            try:
                mic_q.put_nowait(bytes(indata))
            except asyncio.QueueFull:
                pass

        async def mic_sender():
            with sd.RawInputStream(samplerate=SR, channels=1, dtype=DTYPE, blocksize=FRAMES, callback=on_mic):
                while not stop.is_set():
                    chunk = await mic_q.get()
                    lvl = chunk_peak_level(chunk)
                    state["mic_level"] = lvl
                    state["mic_bytes"] += len(chunk)

                    # Barge-in: only if a response is active
                    if state["response_active"] and lvl >= BAR_GE_THRESH:
                        now_ms = time.time() * 1000.0
                        if now_ms - state["last_cancel_ts"] >= CANCEL_COOLDOWN_MS:
                            try:
                                await conn.response.cancel()
                                # Truncate unplayed assistant audio on server to keep context aligned
                                try:
                                    last_id = state.get("last_assistant_item_id")
                                    if last_id:
                                        await conn.conversation.item.truncate(
                                            item_id=last_id,
                                            content_index=0,
                                            audio_end_ms=0
                                        )
                                except Exception:
                                    pass
                                # Start a short suppression window for late deltas
                                state["cancel_guard_until"] = now_ms + SUPPRESS_AFTER_CANCEL_MS
                                print("\n[barge-in] canceled assistant.", flush=True)
                            except Exception:
                                # benign if no active response
                                pass
                            state["last_cancel_ts"] = now_ms
                            state["response_active"] = False
                            state["response_inflight"] = False
                            # Flush any already-queued audio
                            try:
                                while not audio_out_q.empty():
                                    audio_out_q.get_nowait()
                            except Exception:
                                pass

                    # Send mic chunk
                    b64 = base64.b64encode(chunk).decode("ascii")
                    await conn.input_audio_buffer.append(audio=b64)

        async def audio_writer():
            with sd.RawOutputStream(samplerate=SR, channels=1, dtype=DTYPE, blocksize=FRAMES) as out:
                while not stop.is_set():
                    try:
                        audio_bytes = await asyncio.wait_for(audio_out_q.get(), timeout=0.1)
                    except asyncio.TimeoutError:
                        continue
                    try:
                        out.write(audio_bytes)
                    except Exception:
                        pass

        async def event_pump():
            async for event in conn:
                if stop.is_set():
                    break
                et = getattr(event, "type", None)
                now_ms = time.time() * 1000.0

                if et == "session.created":
                    # nothing to do; included for completeness
                    pass

                elif et == "input_audio_buffer.committed":
                    # Create exactly once per committed user turn
                    if not state.get("response_inflight", False):
                        state["response_inflight"] = True
                        asyncio.create_task(conn.response.create())

                # Track assistant items for later truncate()
                elif et == "response.output_item.added":
                    try:
                        item = getattr(event, "item", None)
                        if item is not None:
                            state["last_assistant_item_id"] = getattr(item, "id", None) or state["last_assistant_item_id"]
                    except Exception:
                        pass

                elif et == "conversation.item.created":
                    try:
                        item = getattr(event, "item", None)
                        # For assistant messages created during response
                        if item and getattr(item, "type", None) == "message" and getattr(item, "role", None) == "assistant":
                            state["last_assistant_item_id"] = getattr(item, "id", None)
                    except Exception:
                        pass

                elif et == "response.audio.delta":
                    # Drop stale deltas for a short window after cancel
                    if now_ms < state["cancel_guard_until"]:
                        continue
                    state["response_active"] = True
                    audio_bytes = base64.b64decode(event.delta)
                    state["spk_level"] = chunk_peak_level(audio_bytes)
                    state["spk_bytes"] += len(audio_bytes)
                    try:
                        audio_out_q.put_nowait(audio_bytes)
                    except asyncio.QueueFull:
                        # drop oldest tiny chunk
                        try:
                            _ = audio_out_q.get_nowait()
                            audio_out_q.put_nowait(audio_bytes)
                        except Exception:
                            pass

                elif et == "response.audio.done":
                    state["response_active"] = False
                    state["response_inflight"] = False

                elif et == "response.text.delta":
                    if now_ms < state["cancel_guard_until"]:
                        continue
                    delta = event.delta or ""
                    state["last_assistant"] += delta
                    print(delta, end="", flush=True)

                elif et == "response.text.done":
                    print()
                    # text-only sessions: clear inflight as a safeguard
                    state["response_inflight"] = False

                elif et == "response.done":
                    state["response_active"] = False
                    state["response_inflight"] = False

                elif et == "conversation.item.input_audio_transcription.completed":
                    tr = getattr(event, "transcript", "") or ""
                    state["last_you"] = tr
                    print(f"\nYOU: {tr}")

                elif et == "error":
                    # Ignore benign cancel errors; show others
                    err = getattr(event, "error", {})
                    msg = getattr(err, "message", "") or str(err)
                    code = getattr(err, "code", "") or ""
                    if code != "response_cancel_not_active":
                        print(f"\n[realtime error] {msg}")

        async def key_pump():
            while not stop.is_set():
                line = await key_q.get()
                if line.strip().lower() == "q":
                    # Best-effort cancel & close
                    stop.set()
                    try:
                        await conn.response.cancel()
                    except Exception:
                        pass
                    try:
                        await conn.close()
                    except Exception:
                        pass
                    break

        # Ctrl+C → stop
        try:
            loop.add_signal_handler(signal.SIGINT, stop.set)
        except NotImplementedError:
            pass

        tasks = [
            asyncio.create_task(mic_sender()),
            asyncio.create_task(audio_writer()),
            asyncio.create_task(event_pump()),
            asyncio.create_task(ui_task()),
            asyncio.create_task(key_pump()),
        ]

        await stop.wait()
        for t in tasks:
            t.cancel()
        # Quietly drain
        await asyncio.sleep(0.05)

        # Restore terminal settings if we changed them (POSIX)
        if _orig_termios is not None and _stdin_fd is not None and os.name != "nt":
            try:
                import termios
                termios.tcsetattr(_stdin_fd, termios.TCSADRAIN, _orig_termios)
            except Exception:
                pass

if __name__ == "__main__":
    api_key = os.getenv("OPENAI_API_KEY") or os.getenv("OPENAI_AI_KEY")
    if api_key:
        os.environ["OPENAI_API_KEY"] = api_key
    if not os.getenv("OPENAI_API_KEY"):
        raise SystemExit("Set OPENAI_API_KEY or OPENAI_AI_KEY in your environment")
    asyncio.run(main())
