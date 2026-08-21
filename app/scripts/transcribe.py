#!/usr/bin/env python3
"""HoosierSDR transcription worker.

Reads JSON lines on stdin: {"id": 12, "path": "/x.wav"}; writes JSON lines
on stdout: {"id": 12, "text": "...", "model": "faster-whisper/base",
"secs": 1.23} or {"id": 12, "error": "..."}. One model load per process.

Engines: faster-whisper (CTranslate2; runs well on modest CPUs) or
openai-whisper (PyTorch). Chosen with --engine; --model names the size.
"""
import argparse, json, sys, time

ap = argparse.ArgumentParser()
ap.add_argument("--engine", default="faster-whisper", choices=["faster-whisper", "openai-whisper"])
ap.add_argument("--model", default="base")
ap.add_argument("--language", default="en")
ap.add_argument("--device", default="auto")
ap.add_argument("--compute", default="auto")
ap.add_argument("--probe", action="store_true", help="report available engines and exit")
ap.add_argument("--download", action="store_true", help="fetch/load the model once and exit")
a = ap.parse_args()

def out(obj):
    sys.stdout.write(json.dumps(obj) + "\n"); sys.stdout.flush()

if a.probe:
    engines = []
    for name, mod in (("faster-whisper", "faster_whisper"), ("openai-whisper", "whisper")):
        try:
            __import__(mod); engines.append(name)
        except Exception:
            pass
    out({"engines": engines, "python": sys.version.split()[0]}); sys.exit(0)

try:
    if a.engine == "faster-whisper":
        from faster_whisper import WhisperModel
        compute = a.compute if a.compute != "auto" else ("int8" if a.device == "cpu" else "default")
        model = WhisperModel(a.model, device=a.device, compute_type=compute)
        def run(path):
            segs, info = model.transcribe(path, language=a.language or None, beam_size=5,
                                          vad_filter=True, condition_on_previous_text=False)
            return " ".join(s.text.strip() for s in segs).strip()
    else:
        import whisper
        model = whisper.load_model(a.model)
        def run(path):
            r = model.transcribe(path, language=a.language or None, fp16=False)
            return r.get("text", "").strip()
except Exception as e:
    out({"fatal": f"{a.engine} {a.model}: {e}"}); sys.exit(2)

if a.download:
    out({"downloaded": True, "model": f"{a.engine}/{a.model}"}); sys.exit(0)

out({"ready": True, "model": f"{a.engine}/{a.model}"})
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        req = json.loads(line)
        t0 = time.time()
        text = run(req["path"])
        out({"id": req["id"], "text": text, "model": f"{a.engine}/{a.model}", "secs": round(time.time() - t0, 2)})
    except Exception as e:
        out({"id": req.get("id") if isinstance(req, dict) else None, "error": str(e)})
