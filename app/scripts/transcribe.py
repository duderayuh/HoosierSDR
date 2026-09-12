#!/usr/bin/env python3
"""HoosierSDR transcription worker.

Reads JSON lines on stdin: {"id": 12, "path": "/x.wav"}; writes JSON lines
on stdout: {"id": 12, "text": "...", "model": "faster-whisper/base",
"secs": 1.23} or {"id": 12, "error": "..."}. One model load per process.

Engines: faster-whisper (CTranslate2; runs well on modest CPUs),
openai-whisper (PyTorch), or mlx-whisper (Apple MLX: runs the model on an
Apple-silicon GPU with next to no CPU — the engine to use on a Mac that is
also decoding a radio site in real time). Chosen with --engine; --model
names the size.
"""
import argparse, json, os, signal, sys, threading, time

ap = argparse.ArgumentParser()
ap.add_argument("--engine", default="faster-whisper", choices=["faster-whisper", "openai-whisper", "mlx-whisper"])
ap.add_argument("--model", default="base")
ap.add_argument("--language", default="en")
ap.add_argument("--device", default="auto")
ap.add_argument("--compute", default="auto")
ap.add_argument("--probe", action="store_true", help="report available engines and exit")
ap.add_argument("--download", action="store_true", help="fetch/load the model once and exit")
a = ap.parse_args()

def out(obj):
    sys.stdout.write(json.dumps(obj) + "\n"); sys.stdout.flush()

# The app is the only reader of these answers. When it goes away (quit, a
# crash, a dev rebuild's SIGKILL) stdin's end is only noticed between jobs,
# and a transcription stuck inside the engine would keep the GPU busy with
# no one waiting for it — so notice the parent going, and go too.
_parent = os.getppid()


def _orphan_watch():
    while True:
        time.sleep(2)
        if os.getppid() != _parent:
            os._exit(0)


threading.Thread(target=_orphan_watch, daemon=True).start()

if a.probe:
    engines = []
    for name, mod in (("faster-whisper", "faster_whisper"), ("openai-whisper", "whisper"), ("mlx-whisper", "mlx_whisper")):
        try:
            __import__(mod); engines.append(name)
        except Exception:
            pass
    out({"engines": engines, "python": sys.version.split()[0]}); sys.exit(0)

try:
    if a.engine == "faster-whisper":
        from faster_whisper import WhisperModel
        # There is no GPU path for faster-whisper on macOS, so "auto" is the
        # CPU there — say so, or CTranslate2 picks the model's float16 and
        # emulates it (it warns that the CPU has no efficient float16), which
        # measured 5 s at 10-13 cores for a 21 s call. int8 is the CPU-native
        # choice. And cap the threads: the decoder and the loudspeaker share
        # this machine, and a transcription that grabs every core stalls the
        # radio stream (dropped blocks → holes in the very next calls).
        import os, platform
        device = a.device
        if device == "auto" and platform.system() == "Darwin":
            device = "cpu"
        compute = a.compute if a.compute != "auto" else ("int8" if device == "cpu" else "default")
        threads = max(2, min(8, (os.cpu_count() or 4) // 3))
        model = WhisperModel(a.model, device=device, compute_type=compute, cpu_threads=threads, num_workers=1)
        def run(path):
            segs, info = model.transcribe(path, language=a.language or None, beam_size=5,
                                          vad_filter=True, condition_on_previous_text=False)
            return " ".join(s.text.strip() for s in segs).strip()
    elif a.engine == "mlx-whisper":
        import mlx_whisper
        # Models come from the mlx-community conversions on Hugging Face; a
        # size name maps onto the matching repo, and a full "org/name" is
        # passed through for anything else.
        name = a.model
        if "/" in name:
            repo = name
        elif name in ("turbo", "large-v3-turbo"):
            repo = "mlx-community/whisper-turbo"
        elif name.startswith("distil-"):
            repo = f"mlx-community/{name}-mlx"
        else:
            repo = f"mlx-community/whisper-{name}-mlx"
        def run(path):
            r = mlx_whisper.transcribe(path, path_or_hf_repo=repo, language=a.language or None,
                                       condition_on_previous_text=False)
            return r.get("text", "").strip()
        if a.download:
            # Fetching happens on first use; do one short transcription so
            # the weights land in the cache before live calls need them.
            import numpy as np
            run(np.zeros(16000, dtype=np.float32))
    else:
        import whisper
        # No device chosen here: on a Mac, PyTorch's Metal backend cannot run
        # an op this model needs (sparse alignment heads), so this engine is
        # CPU-only there — and it spreads over every core. Prefer mlx-whisper
        # on Apple silicon, faster-whisper elsewhere.
        model = whisper.load_model(a.model)
        def run(path):
            r = model.transcribe(path, language=a.language or None, fp16=False)
            return r.get("text", "").strip()
except Exception as e:
    out({"fatal": f"{a.engine} {a.model}: {e}"}); sys.exit(2)

if a.download:
    out({"downloaded": True, "model": f"{a.engine}/{a.model}"}); sys.exit(0)

class JobTimeout(BaseException):
    """Raised by the alarm. A BaseException, so no `except Exception` inside
    an engine can swallow it."""


def _alarm(signum, frame):
    raise JobTimeout()


signal.signal(signal.SIGALRM, _alarm)

out({"ready": True, "model": f"{a.engine}/{a.model}"})
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req, limit = None, 0
    try:
        req = json.loads(line)
        # The app's time limit for this job. Whisper's decode loop can stop
        # advancing on a garbled clip: a window whose last timestamp token is
        # 0.00 adds nothing to `seek`, and it decodes the same window again,
        # for ever (a 0.9 s clip held the worker for minutes, and every call
        # queued behind it went untranscribed).
        limit = int(req.get("timeout") or 0)
        t0 = time.time()
        if limit > 0:
            signal.alarm(limit)
        try:
            text = run(req["path"])
        finally:
            signal.alarm(0)
        out({"id": req["id"], "text": text, "model": f"{a.engine}/{a.model}", "secs": round(time.time() - t0, 2)})
    except JobTimeout:
        out({"id": req.get("id") if isinstance(req, dict) else None,
             "error": f"gave up after {limit} s: the model stopped advancing through the audio"})
    except Exception as e:
        out({"id": req.get("id") if isinstance(req, dict) else None, "error": str(e)})
