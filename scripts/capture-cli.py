#!/usr/bin/env python3
"""Capture real CLI output against an isolated local fixture.

Usage: python3 scripts/capture-cli.py BEFORE_BINARY AFTER_BINARY OUTPUT_DIR
Requires Pillow and a fake Codex executable from tests/fixtures/fake_codex.rs
at /tmp/crow-capture-tools/codex. No live installation or GitHub is touched.
Images render captured terminal text; the adjacent .txt files retain full output.
"""
import html
import json
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import sys
import tempfile
import textwrap
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from PIL import Image, ImageDraw, ImageFont


NOW = int(time.time() * 1000)
SETTINGS = {"model": "provider-default", "effort": "medium", "subagents": {"mode": "inherit", "max": 8}, "retry": {"mode": "fixed", "count": 10, "delayMs": 5000}, "timeoutMs": 0}
REPO = {"name": "acme/website", "installation": 12345, "worker": "review-worker", "policy": "selected", "authors": ["alice", "bob"], "requesters": ["alice"], "settings": {}, "enrolledAt": NOW - 86400000, "excluded": [12, 13]}
JOBS = [{"id": f"review-{i}", "key": f"acme/website#{n}", "repo": "acme/website", "number": n, "author": "alice", "head": "a" * 40, "target": "main", "worker": "review-worker", "state": state, "session": f"session-{i}", "settings": SETTINGS, "retries": 0, "nextAt": 0, "createdAt": NOW - 600000, "updatedAt": NOW - i * 60000, "reason": reason} for i, (n, state, reason) in enumerate([(42, "reviewing", None), (41, "paused", "Codex subscription login needs attention on the worker. Then resume this review."), (40, "held", "Waiting for release after catch-up."), (39, "completed", None)], 1)]
STATUS = {"repos": [REPO], "workers": [{"id": "review-worker", "lastSeen": NOW - 10000, "active": ["review-1"], "defaults": dict(SETTINGS, concurrency=3)}], "jobs": JOBS, "draining": False}


class Fixture(BaseHTTPRequestHandler):
    status_value = STATUS

    def log_message(self, *_):
        pass

    def do_GET(self):
        self.respond(self.status_value if self.path == "/admin/status" else {"ok": True, "service": "crow", "configured": True})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or "{}")
        action = self.path.rsplit("/", 1)[-1]
        result = {
            "ping": {"ok": True, "id": "review-worker"},
            "maintenance": {"jobs": [], "retentionDays": 7},
            "pair": body,
            "enroll": REPO,
            "config-repo": dict(REPO, **{k: v for k, v in body.items() if k != "repo"}),
            "review": dict(JOBS[0], state="queued"),
            "restart": dict(JOBS[0], state="queued"),
            "resume": dict(JOBS[0], state="queued"),
            "pause": {"paused": True},
            "catch-up": {"acme/website": {"queued": 2, "held": 0}},
            "release": {"released": 3},
            "drain": {"draining": True},
            "undrain": {"draining": False},
            "cleanup": {"cleaned": True},
        }.get(action, {"ok": True})
        self.respond(result)

    def respond(self, value):
        body = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def picture(path, title, command, output):
    font = ImageFont.truetype("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf", 16)
    lines = []
    for line in ("$ " + command + "\n\n" + output).splitlines():
        lines.extend(textwrap.wrap(line, 100, replace_whitespace=False, drop_whitespace=False) or [""])
    image = Image.new("RGB", (1040, 68 + 23 * len(lines)), "#101820")
    draw = ImageDraw.Draw(image)
    draw.rectangle((0, 0, 1040, 43), fill="#24313d")
    draw.text((24, 12), title, font=font, fill="#dce7ee")
    for i, line in enumerate(lines):
        draw.text((24, 57 + i * 23), line, font=font, fill="#9ee6bb" if i == 0 else "#e4eaf0")
    image.save(path)


def gallery(out):
    entries = sorted((p.name[:-11] for p in out.glob("*-before.txt")), key=lambda name: (name != "status", name.startswith("help"), name))
    cards = []
    for name in entries:
        if not (out / f"{name}-after.txt").exists():
            continue
        cards.append(f'<section id="{name}"><h2>{html.escape(name.replace("-", " "))}</h2><div class="pair">' + "".join(f'<article><h3>{side.title()}</h3><a href="{name}-{side}.png"><img loading="lazy" src="{name}-{side}.png" alt="{name} {side}"></a><a href="{name}-{side}.txt">Full transcript</a></article>' for side in ["before", "after"]) + '</div></section>')
    (out / "index.html").write_text('<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width"><title>Crow interface review</title><style>body{font:16px system-ui;background:#f3f5f7;color:#162330;margin:32px}h1{margin-bottom:8px}section{margin:48px 0}nav{position:sticky;top:0;background:#f3f5f7;padding:12px 0}input{font:inherit;padding:10px;width:min(480px,90%);border:1px solid #8795a3;border-radius:5px}.pair{display:grid;grid-template-columns:1fr 1fr;gap:20px}article{min-width:0}img{width:100%;max-height:650px;object-fit:cover;object-position:top;border-radius:8px}a{color:#155ca2}h3{font-size:16px}@media(max-width:800px){.pair{grid-template-columns:1fr}}</style><h1>Crow interface review</h1><p>Before and after, using the same local sample data. Terminal images render captured output. Click an image for its full size or read its transcript. Browser pages and setup excerpts are identified in their transcripts.</p><nav><label for=filter>Find a command or screen </label><input id=filter type=search placeholder="For example: status, setup, help"></nav>' + "".join(cards) + '<script>document.getElementById("filter").addEventListener("input",event=>{const query=event.target.value.toLowerCase();document.querySelectorAll("section").forEach(section=>{section.hidden=!section.id.includes(query)})});</script></html>')


def main():
    before, after, output = sys.argv[1:]
    out = Path(output).resolve()
    out.mkdir(parents=True, exist_ok=True)
    server = ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    port = server.server_address[1]
    commands = ["version", "install", "setup", "run", "start", "stop", "service-restart", "status", "doctor", "logs", "login", "models", "enroll", "policy", "repo-config", "review", "pause", "resume", "restart", "catch-up", "release", "pair", "config", "cleanup", "update", "drain", "undrain", "backup", "restore"]
    for side, binary in [("before", before), ("after", after)]:
        with tempfile.TemporaryDirectory(prefix="crow-ui-") as tmp:
            root = Path(tmp)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            for name, body in {"systemctl": f'case "$*" in *is-enabled*) echo enabled;; *is-active*) echo active;; *MainPID*) echo {os.getpid()};; esac', "gh": 'case "$*" in *token*) echo fixture-github-token;; *) echo \'{"login":"alice"}\';; esac', "journalctl": 'echo "Sep 16 10:00:00 crow: Crow service and worker running."'}.items():
                file = bin_dir / name
                file.write_text("#!/bin/sh\n" + body + "\n")
                file.chmod(0o755)
            cfg = {"version": 1, "role": "both", "operator": "alice", "publicUrl": "https://crow.example.com", "port": port, "bind": "127.0.0.1", "adminToken": "fixture-admin-token-" * 3, "serviceUrl": f"http://127.0.0.1:{port}", "worker": dict(SETTINGS, id="review-worker", token="fixture-worker-token-" * 3, concurrency=3, codex="/tmp/crow-capture-tools/codex", codexHome=str(root / "codex")), "catchUp": {"enabled": True, "threshold": 10}, "auditIntervalMs": 3600000, "retentionDays": 7, "ingress": {"type": "funnel"}, "app": None}
            def save():
                (root / "config.json").write_text(json.dumps(cfg))
            save()
            (root / "update-status.json").write_text(json.dumps({"checkedAt": NOW, "available": False}))
            (root / "runtime.lock").write_text(json.dumps({"pid": os.getpid()}))
            (root / "ready.json").write_text(json.dumps({"pid": os.getpid(), "version": "0.3.0"}))
            env = dict(os.environ, CROW_HOME=tmp, HOME=tmp, PATH=str(bin_dir) + ":" + os.environ["PATH"], NO_COLOR="1", COLUMNS="100")
            def capture(name, args, **kw):
                run = subprocess.run([str(Path(binary).resolve()), *args], env=env, capture_output=True, text=True, timeout=45, **kw)
                text = run.stdout + run.stderr
                text = re.sub(r"\x1b\[[0-9;]*m", "", text).replace(tmp, "/home/alice/.local/share/crow").replace(str(port), "8787")
                command = ("crow " + " ".join(args)).replace(tmp, "/home/alice/.local/share/crow")
                (out / f"{name}-{side}.txt").write_text("$ " + command + "\n\n" + text)
                picture(out / f"{name}-{side}.png", side.title(), command, text)
                print(side, name, run.returncode, flush=True)
            capture("help", ["--help"])
            for command in commands:
                capture("help-" + command, [command, "--help"])
            for command in ["status", "config", "models", "pair", "drain", "undrain", "cleanup", "catch-up", "release", "start", "stop", "service-restart", "logs"]:
                capture(command, [command])
            capture("enroll", ["enroll", "acme/website"])
            capture("policy", ["policy", "acme/website", "--authors", "alice,bob"])
            capture("repo-config", ["repo-config", "acme/website", "--json", '{"model":"provider-default","effort":"medium"}'])
            for command in ["review", "pause", "resume", "restart"]:
                capture(command, [command, "acme/website", "42"])
            capture("config-set", ["config", "worker.concurrency", "4"])
            capture("config-model", ["config", "worker.model", "provider-default"])
            Fixture.status_value = {"repos": [], "workers": [], "jobs": [], "draining": False}
            capture("status-empty", ["status"])
            Fixture.status_value = STATUS
            cfg["role"] = "worker"
            save()
            (root / "worker-status.json").write_text(json.dumps({"version": 1, "state": "running", "connection": "connected", "active": [{"id": "review-1", "repo": "acme/website", "number": 42, "state": "reviewing"}], "pid": os.getpid(), "updatedAt": NOW}))
            capture("status-worker", ["status"])
            capture("doctor", ["doctor"])
            capture("doctor-runtime", ["doctor", "--runtime"])
            (root / "runtime.lock").unlink()
            capture("status-stopped", ["status"])
            secret = root / "passphrase"
            secret.write_text("local-screenshot-fixture-passphrase\n")
            archive = root / "crow.backup"
            capture("backup", ["backup", str(archive), "--passphrase-file", str(secret)])
            capture("restore", ["restore", str(archive), "--passphrase-file", str(secret)])
            cfg["role"] = "service"
            cfg["catchUp"]["enabled"] = False
            with socket.socket() as sock:
                sock.bind(("127.0.0.1", 0))
                cfg["port"] = sock.getsockname()[1]
            save()
            (root / "ready.json").unlink(missing_ok=True)
            proc = subprocess.Popen([str(Path(binary).resolve()), "run"], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            deadline = time.monotonic() + 10
            while not (root / "ready.json").exists() and proc.poll() is None and time.monotonic() < deadline:
                time.sleep(.02)
            proc.send_signal(signal.SIGTERM)
            stdout, stderr = proc.communicate(timeout=10)
            (out / f"run-{side}.txt").write_text("$ crow run\n\n" + stdout + stderr)
            picture(out / f"run-{side}.png", side.title(), "crow run", stdout + stderr)
            cfg["serviceUrl"] = "http://127.0.0.1:9"
            save()
            capture("error-service-unavailable", ["status"])
            (root / "config.json").unlink()
            capture("error-unconfigured", ["status"])
            capture("error-invalid-command", ["stats"])
    server.shutdown()
    gallery(out)


if __name__ == "__main__":
    main()
