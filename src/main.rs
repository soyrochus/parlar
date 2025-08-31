// Cargo deps (add to Cargo.toml):
// tokio = { version = "1.30", features = ["full"] }
// tokio-tungstenite = "0.20"
// tungstenite = "0.20"
// futures-util = "0.3"
// serde = { version = "1.0", features = ["derive"] }
// serde_json = "1.0"
// cpal = "0.14"
// crossbeam-channel = "0.5"
// crossterm = "0.27"
// base64 = "0.21"
// anyhow = "1.0"
// dotenvy = "0.15"

use std::collections::VecDeque;
use std::env;
use std::process;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use base64;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, SampleRate, StreamConfig};
use crossbeam_channel::{unbounded, Receiver, Sender};
use crossterm::event::{self, Event as CEvent, KeyCode};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use http::HeaderValue;
use tungstenite::Message;

#[derive(Default)]
struct State {
    // lightweight meters
    mic_level: f32,
    spk_level: f32,
    mic_bytes: usize,
    spk_bytes: usize,

    // latest utterances
    last_user: String,
    last_assistant: String,

    // response lifecycle
    response_active: bool,
    response_inflight: bool,
    last_assistant_item_id: Option<String>,

    // interruption + transcript
    last_cancel_at: Option<Instant>,
    last_user_partial: String,
}

fn chunk_peak_level_i16(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut peak = 0i16;
    for &s in samples {
        let a = s.wrapping_abs();
        if a > peak {
            peak = a;
        }
    }
    (peak as f32 / i16::MAX as f32).min(1.0)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    // ------------------- Config (env) -------------------
    let api_key = env::var("OPENAI_API_KEY")
        .expect("OPENAI_API_KEY must be set (in env or .env)");

    let model = env::var("REALTIME_MODEL").unwrap_or_else(|_| "gpt-realtime".into());
    let voice = env::var("REALTIME_VOICE").unwrap_or_else(|_| "alloy".into());

    let sr_hz: u32 = env::var("SR").ok().and_then(|v| v.parse().ok()).unwrap_or(24_000);
    let chunk_ms: u32 = env::var("CHUNK_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(20);

    // While assistant speaks, gate mic by onset to reduce echo-triggered interrupts
    let onset_peak: f32 = env::var("INT_ONSET_PEAK").ok().and_then(|v| v.parse().ok()).unwrap_or(0.22);
    let onset_min_chunks: usize = env::var("INT_ONSET_MIN_CHUNKS").ok().and_then(|v| v.parse().ok()).unwrap_or(2);
    let cancel_cooldown_ms: u64 = env::var("CANCEL_COOLDOWN_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(400);

    // Server VAD tuning: make the system more patient by default
    let vad_silence_ms: u64 = env::var("TURN_SIL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(350);
    let vad_threshold: f32 = env::var("TURN_VAD_THRESH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.55);

    // Adaptive response delays (in addition to VAD commit)
    let resp_delay_short_ms: u64 = env::var("RESP_DELAY_SHORT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    let resp_delay_long_ms: u64 = env::var("RESP_DELAY_LONG_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(700);

    println!("Parlar Realtime (Rust) — model={model} voice={voice} SR={sr_hz}Hz chunk={chunk_ms}ms");
    println!("Commands: [I] Interrupt  [Q] Quit");

    // ------------------- Audio I/O -------------------
    let host = cpal::default_host();
    let input_device = host
        .default_input_device()
        .expect("No input audio device found");
    let output_device = host
        .default_output_device()
        .expect("No output audio device found");

    // Try to pick a 24 kHz mono config; otherwise fall back to default but keep mono.
    let desired_rate = SampleRate(sr_hz);
    let channels = 1u16;

    let pick_input_cfg = || -> StreamConfig {
        if let Ok(configs) = input_device.supported_input_configs() {
            for range in configs {
                if range.channels() == channels
                    && range.min_sample_rate() <= desired_rate
                    && range.max_sample_rate() >= desired_rate
                {
                    return range.with_sample_rate(desired_rate).config();
                }
            }
        }
        let mut cfg = input_device
            .default_input_config()
            .expect("No default input config")
            .config();
        cfg.channels = channels;
        cfg
    };
    let pick_output_cfg = || -> StreamConfig {
        if let Ok(configs) = output_device.supported_output_configs() {
            for range in configs {
                if range.channels() == channels
                    && range.min_sample_rate() <= desired_rate
                    && range.max_sample_rate() >= desired_rate
                {
                    return range.with_sample_rate(desired_rate).config();
                }
            }
        }
        let mut cfg = output_device
            .default_output_config()
            .expect("No default output config")
            .config();
        cfg.channels = channels;
        cfg
    };

    let mut input_cfg = pick_input_cfg();
    input_cfg.buffer_size = BufferSize::Default;

    let mut output_cfg = pick_output_cfg();
    output_cfg.buffer_size = BufferSize::Default;

    // Shared output audio ring buffer (PCM16)
    let spk_buf: Arc<Mutex<VecDeque<i16>>> = Arc::new(Mutex::new(VecDeque::with_capacity(96_000)));

    // Mic -> network channel (raw PCM16 bytes per chunk)
    let (mic_tx, mic_rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = unbounded();

    let state = Arc::new(Mutex::new(State::default()));

    // Input stream (capture mic)
    let input_sample_format = input_device
        .default_input_config()
        .expect("no default input config")
        .sample_format();

    let frames_per_chunk =
        (input_cfg.sample_rate.0 as u32 * chunk_ms / 1000).max(1) as usize;

    let mic_tx_clone = mic_tx.clone();
    let state_for_input = state.clone();
    let input_stream = match input_sample_format {
        SampleFormat::I16 => input_device.build_input_stream(
            &input_cfg,
            move |data: &[i16], _| {
                // Slice by frames_per_chunk into fixed chunks → convert to bytes
                for frame_chunk in data.chunks(frames_per_chunk) {
                    let peak = chunk_peak_level_i16(frame_chunk);
                    if let Ok(mut st) = state_for_input.lock() {
                        st.mic_level = peak;
                        st.mic_bytes += frame_chunk.len() * 2;
                    }
                    let bytes = unsafe {
                        std::slice::from_raw_parts(
                            frame_chunk.as_ptr() as *const u8,
                            frame_chunk.len() * 2,
                        )
                    };
                    let _ = mic_tx_clone.send(bytes.to_vec());
                }
            },
            |e| eprintln!("Input stream error: {e:?}"),
        )?,
        SampleFormat::F32 => input_device.build_input_stream(
            &input_cfg,
            move |data: &[f32], _| {
                for frame_chunk in data.chunks(frames_per_chunk) {
                    // convert to i16
                    let mut pcm = Vec::with_capacity(frame_chunk.len());
                    for &s in frame_chunk {
                        let v = (s * i16::MAX as f32)
                            .round()
                            .clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                        pcm.push(v);
                    }
                    let peak = chunk_peak_level_i16(&pcm);
                    if let Ok(mut st) = state_for_input.lock() {
                        st.mic_level = peak;
                        st.mic_bytes += pcm.len() * 2;
                    }
                    let bytes = unsafe {
                        std::slice::from_raw_parts(pcm.as_ptr() as *const u8, pcm.len() * 2)
                    };
                    let _ = mic_tx_clone.send(bytes.to_vec());
                }
            },
            |e| eprintln!("Input stream error: {e:?}"),
        )?,
        SampleFormat::U16 => input_device.build_input_stream(
            &input_cfg,
            move |data: &[u16], _| {
                for frame_chunk in data.chunks(frames_per_chunk) {
                    let mut pcm = Vec::with_capacity(frame_chunk.len());
                    for &s in frame_chunk {
                        pcm.push((s as i32 - 32768) as i16);
                    }
                    let peak = chunk_peak_level_i16(&pcm);
                    if let Ok(mut st) = state_for_input.lock() {
                        st.mic_level = peak;
                        st.mic_bytes += pcm.len() * 2;
                    }
                    let bytes = unsafe {
                        std::slice::from_raw_parts(pcm.as_ptr() as *const u8, pcm.len() * 2)
                    };
                    let _ = mic_tx_clone.send(bytes.to_vec());
                }
            },
            |e| eprintln!("Input stream error: {e:?}"),
        )?,
    };
    input_stream.play()?;

    // Output stream (play assistant audio)
    let out_sf = output_device
        .default_output_config()
        .expect("no default output config")
        .sample_format();
    let spk_buf_for_out = spk_buf.clone();
    let state_for_out = state.clone();
    let output_stream = match out_sf {
        SampleFormat::I16 => output_device.build_output_stream(
            &output_cfg,
            move |out: &mut [i16], _| {
                let mut buf = spk_buf_for_out.lock().unwrap();
                for s in out.iter_mut() {
                    *s = buf.pop_front().unwrap_or(0);
                }
                // update level (cheap peak over this callback)
                let peak = chunk_peak_level_i16(out);
                if let Ok(mut st) = state_for_out.lock() {
                    st.spk_level = peak;
                    st.spk_bytes += out.len() * 2;
                }
            },
            |e| eprintln!("Output stream error: {e:?}"),
        )?,
        SampleFormat::F32 => output_device.build_output_stream(
            &output_cfg,
            move |out: &mut [f32], _| {
                let mut buf = spk_buf_for_out.lock().unwrap();
                for s in out.iter_mut() {
                    if let Some(v) = buf.pop_front() {
                        *s = (v as f32) / (i16::MAX as f32);
                    } else {
                        *s = 0.0;
                    }
                }
                // derive level from a temporary i16 vec (approx)
                let tmp: Vec<i16> = out
                    .iter()
                    .map(|f| (f * i16::MAX as f32) as i16)
                    .collect();
                let peak = chunk_peak_level_i16(&tmp);
                if let Ok(mut st) = state_for_out.lock() {
                    st.spk_level = peak;
                    st.spk_bytes += out.len() * 2;
                }
            },
            |e| eprintln!("Output stream error: {e:?}"),
        )?,
        SampleFormat::U16 => output_device.build_output_stream(
            &output_cfg,
            move |out: &mut [u16], _| {
                let mut buf = spk_buf_for_out.lock().unwrap();
                for s in out.iter_mut() {
                    if let Some(v) = buf.pop_front() {
                        *s = (v as i32 + 32768).clamp(0, 65535) as u16;
                    } else {
                        *s = 32768;
                    }
                }
                // level (approx)
                let tmp: Vec<i16> = out.iter().map(|u| (*u as i32 - 32768) as i16).collect();
                let peak = chunk_peak_level_i16(&tmp);
                if let Ok(mut st) = state_for_out.lock() {
                    st.spk_level = peak;
                    st.spk_bytes += out.len() * 2;
                }
            },
            |e| eprintln!("Output stream error: {e:?}"),
        )?,
    };
    output_stream.play()?;

    // ------------------- WebSocket -------------------
    let url = format!("wss://api.openai.com/v1/realtime?model={}", model);
    let mut request = url
        .as_str()
        .into_client_request()
        .expect("Failed to build WS request");
    {
        let headers = request.headers_mut();
        headers.insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {}", api_key)).expect("invalid API key"),
        );
        // Historically required during beta; harmless if GA keeps accepting it.
        headers.insert(
            "OpenAI-Beta",
            HeaderValue::from_static("realtime=v1"),
        );
    }

    println!("Connecting to OpenAI Realtime…");
    let (ws_stream, _) = connect_async(request).await.expect("WS connect failed");
    println!("Connected — speak to talk; press I to interrupt, Q to quit.");
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    // Configure session: audio+text, server VAD (manual response.create), PCM16 in/out, voice
    let session_update = json!({
        "type": "session.update",
        "session": {
            "modalities": ["audio", "text"],
            "voice": voice,
            "instructions": "You are a concise, helpful assistant.",
            "input_audio_format": "pcm16",
            "output_audio_format": "pcm16",
            // Let server VAD detect end-of-speech, but do NOT auto-create responses
            "turn_detection": {
                "type": "server_vad",
                "threshold": vad_threshold,
                "silence_duration_ms": vad_silence_ms,
                "prefix_padding_ms": 100,
                "create_response": false
            },
            // Realtime's built-in input transcription (to print "User: ...")
            "input_audio_transcription": { "model": "whisper-1" }
        }
    });
    ws_tx.send(Message::Text(session_update.to_string())).await?;

    // Outgoing sender task (forward Text/Binary to WS)
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
    let send_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if let Err(e) = ws_tx.send(msg).await {
                eprintln!("WS send error: {e:?}");
                break;
            }
        }
    });

    // Thread: mic → input_audio_buffer.append (simple onset gate while speaking)
    let out_tx_audio = out_tx.clone();
    let state_for_mic = state.clone();
    std::thread::spawn(move || {
        let mut loud_consecutive: usize = 0;
        while let Ok(bytes) = mic_rx.recv() {
            // compute peak of this chunk
            let peak = {
                let samples = unsafe {
                    std::slice::from_raw_parts(bytes.as_ptr() as *const i16, bytes.len() / 2)
                };
                chunk_peak_level_i16(samples)
            };

            // update mic meter
            if let Ok(mut st) = state_for_mic.lock() {
                st.mic_level = peak;
                st.mic_bytes += bytes.len();
            }

            // Only gate while the assistant is speaking to avoid echo false-positives
            let speaking = state_for_mic
                .lock()
                .map(|s| s.response_active || s.response_inflight)
                .unwrap_or(false);
            if speaking {
                if peak >= onset_peak { loud_consecutive += 1; } else { loud_consecutive = 0; }
                if loud_consecutive < onset_min_chunks { continue; }
            } else {
                loud_consecutive = 0;
            }

            // forward mic chunk
            let b64 = base64::encode(&bytes);
            let ev = json!({"type": "input_audio_buffer.append", "audio": b64});
            if out_tx_audio.send(Message::Text(ev.to_string())).is_err() { break; }
        }
    });

    // Thread: keyboard (I=interrupt, Q=quit) — macOS/Linux
    {
        let out_tx_ctrl = out_tx.clone();
        let spk_buf_ctrl = spk_buf.clone();
        let state_ctrl = state.clone();
        std::thread::spawn(move || {
            let _ = crossterm::terminal::enable_raw_mode();
            loop {
                if let Ok(CEvent::Key(k)) = event::read() {
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Char('Q') => {
                            println!("\nQuit.");
                            process::exit(0);
                        }
                        KeyCode::Char('i') | KeyCode::Char('I') => {
                            let _ = out_tx_ctrl.send(Message::Text(
                                json!({"type": "response.cancel"}).to_string(),
                            ));
                            if let Some(item_id) =
                                state_ctrl.lock().unwrap().last_assistant_item_id.clone()
                            {
                                let _ = out_tx_ctrl.send(Message::Text(
                                    json!({
                                        "type": "conversation.item.truncate",
                                        "item_id": item_id,
                                        "content_index": 0,
                                        "audio_end_ms": 0
                                    })
                                    .to_string(),
                                ));
                            }
                            if let Ok(mut q) = spk_buf_ctrl.lock() {
                                q.clear();
                            }
                            eprintln!("\n[interrupt] assistant canceled.");
                        }
                        _ => {}
                    }
                }
            }
        });
    }

    // --------------- Incoming events loop ---------------
    let state_for_rx = state.clone();
    let spk_buf_for_rx = spk_buf.clone();

    // Print a tiny status line once
    println!("--- live ---");

    while let Some(msg) = ws_rx.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                eprintln!("WS recv error: {e:?}");
                break;
            }
        };
        if !msg.is_text() {
            continue;
        }
        let text = msg.into_text().unwrap_or_default();
        let Ok(evt) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let et = evt["type"].as_str().unwrap_or("");

        match et {
            "session.created" => { /* no-op */ }
            "error" => {
                let code = evt["error"]["code"].as_str().unwrap_or("");
                let msg = evt["error"]["message"].as_str().unwrap_or("");
                if code != "response_cancel_not_active" {
                    eprintln!("\n[realtime error] {code} {msg}");
                }
            }

            // Server VAD: when the buffer is committed, schedule exactly one response
            "input_audio_buffer.committed" => {
                // schedule response after adaptive pause
                let (out, st_arc) = (out_tx.clone(), state_for_rx.clone());
                let delay_ms = {
                    let st = st_arc.lock().unwrap();
                    let u = st.last_user.clone();
                    if u.ends_with('.') || u.ends_with('!') || u.ends_with('?') {
                        resp_delay_short_ms
                    } else {
                        resp_delay_long_ms
                    }
                };
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    let mut st = st_arc.lock().unwrap();
                    if !st.response_inflight && !st.response_active {
                        st.response_inflight = true;
                        let _ = out.send(Message::Text(json!({"type":"response.create"}).to_string()));
                    }
                });
            }

            // Track assistant message item id for truncate
            "response.output_item.added" => {
                if let Some(id) = evt["item"]["id"].as_str() {
                    state_for_rx.lock().unwrap().last_assistant_item_id =
                        Some(id.to_string());
                }
            }
            "conversation.item.created" => {
                let role = evt["item"]["role"].as_str().unwrap_or("");
                if role == "assistant" {
                    if let Some(id) = evt["item"]["id"].as_str() {
                        state_for_rx.lock().unwrap().last_assistant_item_id =
                            Some(id.to_string());
                    }
                } else if role == "user" {
                    // Show the finalized transcript/text for the user turn, but do not schedule
                    // response here; rely on input_audio_buffer.committed for turn-taking.
                    if let Some(s) = evt["item"]["content"][0]["transcript"].as_str() {
                        println!("\nUser: {}", s);
                        state_for_rx.lock().unwrap().last_user = s.to_string();
                    } else if let Some(s) = evt["item"]["content"][0]["text"].as_str() {
                        println!("\nUser: {}", s);
                        state_for_rx.lock().unwrap().last_user = s.to_string();
                    }
                }
            }

            // Assistant audio streaming
            "response.audio.delta" => {
                if let Some(b64) = evt["delta"].as_str() {
                    if let Ok(bytes) = base64::decode(b64) {
                        let samples = unsafe {
                            std::slice::from_raw_parts(
                                bytes.as_ptr() as *const i16,
                                bytes.len() / 2,
                            )
                        };
                        {
                            let mut st = state_for_rx.lock().unwrap();
                            st.response_active = true;
                        }
                        // push to speaker ring buffer
                        let mut rb = spk_buf_for_rx.lock().unwrap();
                        rb.extend(samples.iter().copied());
                    }
                }
            }
            "response.audio.done" => {
                let mut st = state_for_rx.lock().unwrap();
                st.response_active = false;
                st.response_inflight = false;
            }

            // Assistant text streaming
            "response.text.delta" => {
                if let Some(delta) = evt["delta"].as_str() {
                    print!("{}", delta);
                    use std::io::Write;
                    std::io::stdout().flush().ok();
                    state_for_rx.lock().unwrap().last_assistant.push_str(delta);
                }
            }
            "response.text.done" => {
                println!();
                state_for_rx.lock().unwrap().response_inflight = false;
            }
            "response.done" => {
                let mut st = state_for_rx.lock().unwrap();
                st.response_active = false;
                st.response_inflight = false;
            }

            // Server indicates start of user speech — cancel and flush audio
            "input_audio_buffer.speech_started" => {
                let mut st = state_for_rx.lock().unwrap();
                if st.response_active || st.response_inflight {
                    st.response_active = false;
                    st.response_inflight = false;
                    st.last_cancel_at = Some(Instant::now());
                    drop(st);
                    let _ = out_tx.send(Message::Text(json!({"type":"response.cancel"}).to_string()));
                    if let Some(item_id) = state_for_rx.lock().unwrap().last_assistant_item_id.clone() {
                        let _ = out_tx.send(Message::Text(json!({
                            "type":"conversation.item.truncate",
                            "item_id": item_id,
                            "content_index": 0,
                            "audio_end_ms": 0
                        }).to_string()));
                    }
                    let mut q = spk_buf_for_rx.lock().unwrap();
                    q.clear();
                }
            }

            // When enabled in session: finalized input transcript event
            "conversation.item.input_audio_transcription.completed" => {
                if let Some(tr) = evt["transcript"].as_str() {
                    println!("\nUser: {}", tr);
                    let mut st = state_for_rx.lock().unwrap();
                    st.last_user = tr.to_string();
                    st.last_user_partial.clear();
                }
            }

            // Incremental transcription deltas (for continuous recognition + barge-in keywords)
            "conversation.item.input_audio_transcription.delta" => {
                if let Some(delta) = evt["delta"].as_str() {
                    let mut st = state_for_rx.lock().unwrap();
                    st.last_user_partial.push_str(delta);
                    let speaking = st.response_active || st.response_inflight;
                    let now = Instant::now();
                    let cooldown_ok = st
                        .last_cancel_at
                        .map(|t| now.duration_since(t) >= Duration::from_millis(cancel_cooldown_ms))
                        .unwrap_or(true);
                    let text_lc = st.last_user_partial.to_lowercase();
                    let contains_hot = text_lc.contains(" stop")
                        || text_lc.starts_with("stop")
                        || text_lc.contains(" wait")
                        || text_lc.contains(" hold on")
                        || text_lc.contains(" hey");
                    if speaking && cooldown_ok && contains_hot {
                        st.last_cancel_at = Some(now);
                        drop(st);
                        let _ = out_tx
                            .send(Message::Text(json!({"type":"response.cancel"}).to_string()));
                        if let Some(item_id) = state_for_rx.lock().unwrap().last_assistant_item_id.clone() {
                            let _ = out_tx.send(Message::Text(
                                json!({"type":"conversation.item.truncate","item_id":item_id,"content_index":0,"audio_end_ms":0}).to_string()
                            ));
                        }
                        if let Ok(mut q) = spk_buf_for_rx.lock() { q.clear(); }
                        let mut st2 = state_for_rx.lock().unwrap();
                        st2.last_user_partial.clear();
                        st2.response_active = false;
                        st2.response_inflight = false;
                        eprintln!("\n[interrupt:keyword] assistant canceled.");
                    }
                }
            }

            _ => { /* ignore others */ }
        }
    }

    drop(out_tx);
    let _ = send_task.await;

    println!("Connection closed.");
    Ok(())
}
