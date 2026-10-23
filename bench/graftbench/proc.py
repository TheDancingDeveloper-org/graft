"""Running commands with timing. Tokens never go in argv: graft and
graft-mirror get `--token-file`, and nothing here prints a token."""

from __future__ import annotations

import os
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path


class StepError(RuntimeError):
    """A command the harness relies on failed."""


@dataclass
class Ran:
    rc: int
    out: str
    err: str
    secs: float


def run(
    args: list,
    cwd: Path | None = None,
    env: dict | None = None,
    check: bool = True,
    timeout: float = 3600,
    input: bytes | None = None,
) -> Ran:
    full_env = None
    if env:
        full_env = dict(os.environ)
        full_env.update(env)
    t0 = time.perf_counter()
    try:
        p = subprocess.run(
            [str(a) for a in args],
            cwd=cwd,
            env=full_env,
            input=input,
            capture_output=True,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired as e:
        raise StepError(f"timed out after {timeout}s: {_show(args)}") from e
    secs = time.perf_counter() - t0
    r = Ran(p.returncode, p.stdout.decode(errors="replace"), p.stderr.decode(errors="replace"), secs)
    if check and r.rc != 0:
        raise StepError(f"exit {r.rc}: {_show(args)} (in {cwd})\n{r.err.strip()[-2000:]}")
    return r


def _show(args: list) -> str:
    return " ".join(str(a) for a in args)


def percentile(samples: list[float], q: float) -> float | None:
    """The `q`-th percentile (0-100), nearest-rank with linear interpolation."""
    if not samples:
        return None
    s = sorted(samples)
    if len(s) == 1:
        return s[0]
    k = (len(s) - 1) * q / 100
    lo = int(k)
    hi = min(lo + 1, len(s) - 1)
    return s[lo] + (s[hi] - s[lo]) * (k - lo)


def summarize(samples_s: list[float]) -> dict:
    """Latency summary in milliseconds."""
    ms = [x * 1000 for x in samples_s]
    if not ms:
        return {"n": 0}
    return {
        "n": len(ms),
        "p50_ms": round(percentile(ms, 50), 3),
        "p95_ms": round(percentile(ms, 95), 3),
        "p99_ms": round(percentile(ms, 99), 3),
        "mean_ms": round(sum(ms) / len(ms), 3),
        "max_ms": round(max(ms), 3),
        "total_s": round(sum(samples_s), 3),
    }
