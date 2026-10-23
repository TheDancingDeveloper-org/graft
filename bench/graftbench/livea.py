"""Live awareness: does checking the server before you finish (sync, a
conflict preview, draft overlap) cut conflicts and rework when many agents
edit similar code? A deterministic mode, so the answer is a count, not a
race (WI-2311).

- Script: from a seed, W writers x K steps. Each step is one edit to a
  (file, key) of a small shared module set: the writer's own file, its own
  region of a shared file (a clean merge3 against the others), or a hot key
  every writer may hit (a true conflict). `--overlap` sets the shares. An
  edit adds the step's distinct marker to the key's value, and conflicts are
  resolved by the sorted union of every value in the block, so the expected
  final tree is the same whatever the policy or the order.
- Scheduler: one coordinator runs a logical clock and performs exactly one
  scripted action (edit, commit, sync, land) for one writer per tick. No two
  client calls run at once; the interleaving is round-robin with a seeded
  jitter. The same seed gives the same server states and the same counts.
- Policies on the same script: `late` (land at the end), `sync-early`
  (`graft sync` before each step), and `preview` / `drafts` when
  `graft status --help` offers `--upstream` / `--drafts` (otherwise
  skipped). git baselines: `late` (push at the end) and
  `fetch-rebase-early`.
- Oracle: a pure-Python model replays the same schedule with
  `git merge-file` as the three-way reference. It predicts the conflicting
  paths of every sync and land and the final tree, and records each writer's
  open-draft paths at every tick.
"""

from __future__ import annotations

import hashlib
import random
import re
import shutil
import tempfile
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path

from .mcp import Mcp
from .model import compare, read_tree
from .proc import StepError, run, summarize
from .tools import Git, Graft, Mirror

DIR = "live"
OVERLAPS = {
    # (own file, own region of a shared file, hot key): shares of the steps
    "low": (0.80, 0.15, 0.05),
    "mid": (0.40, 0.35, 0.25),
    "high": (0.10, 0.30, 0.60),
}
SHARED_FILES = 2
HOT_KEYS = 1
PAD = 6  # unchanged lines after every key, so different keys merge cleanly
JITTER = 0.4
GRAFT_POLICIES = ["late", "sync-early", "preview", "drafts"]
GIT_POLICIES = ["late", "fetch-rebase-early"]
MAX_ATTEMPTS = 50

VALUE_RE = re.compile(r'^([A-Za-z0-9_]+) = "([^"]*)"$')


# --- the script -----------------------------------------------------------


@dataclass(frozen=True)
class Step:
    writer: int
    index: int
    kind: str  # own | region | hot
    path: str
    key: str
    marker: str


def own_path(i: int) -> str:
    return f"{DIR}/own_w{i}.py"


def shared_path(s: int) -> str:
    return f"{DIR}/shared_{s}.py"


def render(path: str, keys: list[str], values: dict[str, set[str]] | None = None) -> bytes:
    values = values or {}
    out = [f'"""Live awareness module {path}."""', ""]
    for k in keys:
        out.append(f'{k} = "{",".join(sorted(values.get(k, ())))}"')
        out += [f"PAD_{k}_{n} = {n}" for n in range(PAD)]
    return ("\n".join(out) + "\n").encode()


def edit_text(data: bytes, key: str, marker: str) -> bytes:
    """Adds `marker` to `key`'s value: the line becomes the sorted union."""
    lines = data.decode().split("\n")
    for n, line in enumerate(lines):
        m = VALUE_RE.match(line)
        if m and m.group(1) == key:
            vals = {v for v in m.group(2).split(",") if v} | {marker}
            lines[n] = f'{key} = "{",".join(sorted(vals))}"'
            return "\n".join(lines).encode()
    raise ValueError(f"no key {key} in the file")


def resolve_text(data: bytes) -> tuple[bytes, int, int]:
    """Resolves every conflict block by rule: each key in the block becomes
    the sorted union of its values on every side. Returns the text, the
    blocks resolved and the lines between the outer markers (separators
    included)."""
    out: list[str] = []
    blocks = lines_in = 0
    inside = False
    keys: dict[str, set[str]] = {}
    for line in data.decode().split("\n"):
        if line.startswith("<<<<<<<"):
            inside, keys = True, {}
            blocks += 1
            continue
        if inside and line.startswith(">>>>>>>"):
            inside = False
            out += [f'{k} = "{",".join(sorted(v for v in vs if v))}"' for k, vs in keys.items()]
            continue
        if inside:
            lines_in += 1
            m = VALUE_RE.match(line)
            if m:
                keys.setdefault(m.group(1), set()).update(m.group(2).split(","))
            continue
        out.append(line)
    if inside:
        raise ValueError("an unterminated conflict block")
    return "\n".join(out).encode(), blocks, lines_in


class Script:
    """W writers x K steps from a seed. Every marker is distinct."""

    def __init__(self, seed: int, writers: int, steps: int, overlap: str):
        if overlap not in OVERLAPS:
            raise ValueError(f"overlap is one of {sorted(OVERLAPS)}")
        self.seed, self.writers, self.k, self.overlap = seed, writers, steps, overlap
        rng = random.Random(f"live-awareness:{seed}:{writers}:{steps}:{overlap}")
        own, region, _ = OVERLAPS[overlap]
        self.keys: dict[str, list[str]] = {own_path(i): [f"OWN_W{i}_S{k}" for k in range(steps)] for i in range(writers)}
        for s in range(SHARED_FILES):
            self.keys[shared_path(s)] = [f"HOT_{s}_{h}" for h in range(HOT_KEYS)] + [f"REGION_{s}_W{i}" for i in range(writers)]
        self.steps: list[list[Step]] = []
        for i in range(writers):
            mine = []
            for k in range(steps):
                x = rng.random()
                marker = f"w{i}s{k}"
                if x < own:
                    mine.append(Step(i, k, "own", own_path(i), f"OWN_W{i}_S{k}", marker))
                elif x < own + region:
                    s = rng.randrange(SHARED_FILES)
                    mine.append(Step(i, k, "region", shared_path(s), f"REGION_{s}_W{i}", marker))
                else:
                    s, h = rng.randrange(SHARED_FILES), rng.randrange(HOT_KEYS)
                    mine.append(Step(i, k, "hot", shared_path(s), f"HOT_{s}_{h}", marker))
            self.steps.append(mine)

    def initial(self) -> dict[str, bytes]:
        return {p: render(p, ks) for p, ks in self.keys.items()}

    def expected(self) -> dict[str, bytes]:
        vals: dict[str, dict[str, set[str]]] = {p: {} for p in self.keys}
        for mine in self.steps:
            for st in mine:
                vals[st.path].setdefault(st.key, set()).add(st.marker)
        return {p: render(p, ks, vals[p]) for p, ks in self.keys.items()}

    def kinds(self) -> dict[str, int]:
        out = {"own": 0, "region": 0, "hot": 0}
        for mine in self.steps:
            for st in mine:
                out[st.kind] += 1
        return out

    def digest(self) -> str:
        h = hashlib.sha256()
        for mine in self.steps:
            for st in mine:
                h.update(f"{st.writer}|{st.index}|{st.kind}|{st.path}|{st.key}|{st.marker}\n".encode())
        return h.hexdigest()


# --- programs and the schedule ------------------------------------------


def program(script: Script, writer: int, policy: str) -> list[tuple]:
    """The writer's actions, in order. A `sync` is the policy's probe."""
    acts: list[tuple] = []
    for st in script.steps[writer]:
        if policy in ("sync-early", "fetch-rebase-early"):
            acts.append(("sync",))
        elif policy in ("preview", "drafts"):
            acts.append((policy, st))
        acts += [("edit", st), ("commit", st)]
    acts.append(("land",))
    return acts


def schedule(seed: int, lengths: list[int]) -> list[int]:
    """Which writer acts at each tick: round-robin with a seeded jitter."""
    rng = random.Random(f"live-awareness-schedule:{seed}")
    left = list(lengths)
    alive = [i for i, n in enumerate(left) if n]
    out: list[int] = []
    pos = 0
    while alive:
        skip = rng.randrange(1, len(alive)) if len(alive) > 1 and rng.random() < JITTER else 0
        at = (pos + skip) % len(alive)
        w = alive[at]
        out.append(w)
        left[w] -= 1
        if left[w] == 0:
            alive.pop(at)
            pos = at % len(alive) if alive else 0
        else:
            pos = (at + 1) % len(alive)
    return out


# --- the oracle -----------------------------------------------------------


class Merger:
    """Three-way merges with `git merge-file` (diff3 markers), cached."""

    def __init__(self, git: Git, scratch: Path):
        self.git = git
        self.dir = scratch
        self.dir.mkdir(parents=True, exist_ok=True)
        self.cache: dict[tuple, tuple[bytes, bool]] = {}

    def merge(self, base: bytes, ours: bytes, theirs: bytes) -> tuple[bytes, bool]:
        key = tuple(hashlib.sha256(x).digest() for x in (base, ours, theirs))
        if key in self.cache:
            return self.cache[key]
        paths = []
        for name, data in (("ours", ours), ("base", base), ("theirs", theirs)):
            p = self.dir / name
            p.write_bytes(data)
            paths.append(p)
        r = self.git(["merge-file", "-p", "--diff3", *paths], check=False)
        if r.rc < 0 or r.rc > 127:
            raise StepError(f"git merge-file failed: {r.err.strip()[:300]}")
        out = (r.out.encode(), r.rc > 0)
        self.cache[key] = out
        return out

    def tree3(self, base: dict, ours: dict, theirs: dict) -> tuple[dict, list[str]]:
        out, conflicts = {}, []
        for p in sorted(set(base) | set(ours) | set(theirs)):
            b, o, t = base.get(p), ours.get(p), theirs.get(p)
            if o == b:
                out[p] = t
            elif t == b or o == t:
                out[p] = o
            else:
                merged, conflicted = self.merge(b or b"", o or b"", t or b"")
                out[p] = merged
                if conflicted:
                    conflicts.append(p)
        return {p: v for p, v in out.items() if v is not None}, conflicts


@dataclass
class Counts:
    conflicts_at_land: int = 0
    conflicts_caught_early: int = 0
    conflict_blocks_resolved: int = 0
    conflict_lines_resolved: int = 0
    land_attempts: int = 0
    probe_calls: int = 0
    commits: int = 0
    lands: int = 0

    def as_dict(self) -> dict:
        d = dict(self.__dict__)
        d["rework_regions_rewritten"] = d.pop("conflict_blocks_resolved")
        return d


@dataclass
class Event:
    tick: int
    writer: int
    action: str  # sync | land
    conflicts: list[str]

    def key(self) -> tuple:
        return (self.tick, self.writer, self.action, tuple(self.conflicts))


@dataclass
class _MWriter:
    base: dict
    head: dict
    base_ver: int


class Oracle:
    """Replays a schedule in memory. Knows, for every sync and land, which
    paths conflict, and every writer's open-draft paths at every tick."""

    def __init__(self, script: Script, policy: str, merger: Merger):
        self.script, self.policy, self.m = script, policy, merger
        self.tip = script.initial()
        self.ver = 0
        self.ws = [_MWriter(dict(self.tip), dict(self.tip), 0) for _ in range(script.writers)]
        self.counts = Counts()
        self.events: list[Event] = []
        self.drafts: list[list[list[str]]] = []  # per tick, per writer: open-draft paths

    def _resolve(self, tree: dict, paths: list[str]) -> dict:
        tree = dict(tree)
        for p in paths:
            tree[p], b, n = resolve_text(tree[p])
            self.counts.conflict_blocks_resolved += b
            self.counts.conflict_lines_resolved += n
        return tree

    def act(self, tick: int, w: int, a: tuple) -> None:
        s = self.ws[w]
        kind = a[0]
        if kind == "edit":
            st = a[1]
            s.head[st.path] = edit_text(s.head[st.path], st.key, st.marker)
        elif kind == "commit":
            self.counts.commits += 1
        elif kind == "sync":
            self.counts.probe_calls += 1
            conflicts: list[str] = []
            if s.base_ver != self.ver:
                merged, conflicts = self.m.tree3(s.base, s.head, self.tip)
                if conflicts:
                    self.counts.conflicts_caught_early += 1
                    merged = self._resolve(merged, conflicts)
                    self.counts.commits += 1
                s.head, s.base, s.base_ver = merged, dict(self.tip), self.ver
            self.events.append(Event(tick, w, "sync", conflicts))
        elif kind == "land":
            self.counts.land_attempts += 1
            conflicts = []
            if s.base_ver == self.ver:
                new = s.head
            else:
                new, conflicts = self.m.tree3(s.base, s.head, self.tip)
                if conflicts:
                    # land refused; land --sync writes the markers; resolve,
                    # commit, land again.
                    self.counts.conflicts_at_land += 1
                    self.counts.land_attempts += 2
                    self.counts.commits += 1
                    new = self._resolve(new, conflicts)
            self.tip, self.ver = new, self.ver + 1
            self.counts.lands += 1
            s.base, s.head, s.base_ver = dict(new), dict(new), self.ver
            self.events.append(Event(tick, w, "land", conflicts))
        else:
            raise ValueError(f"the oracle has no model of {kind}")
        self.drafts.append([sorted(p for p in x.head if x.head[p] != x.base.get(p)) for x in self.ws])

    def run(self, sched: list[int], programs: list[list[tuple]]) -> Oracle:
        pc = [0] * len(programs)
        for tick, w in enumerate(sched):
            self.act(tick, w, programs[w][pc[w]])
            pc[w] += 1
        return self

    def draft_overlap(self) -> dict:
        """How often two writers had open drafts on one path."""
        ticks = pairs = 0
        for per in self.drafts:
            seen: dict[str, int] = {}
            for paths in per:
                for p in paths:
                    seen[p] = seen.get(p, 0) + 1
            n = sum(c * (c - 1) // 2 for c in seen.values())
            pairs += n
            ticks += bool(n)
        return {"ticks_with_overlap": ticks, "overlap_pairs": pairs, "ticks": len(self.drafts)}


# --- the runners ----------------------------------------------------------


def _seed_repo(git: Git, mirror: Mirror, files: dict[str, bytes], d: Path, repo: str) -> Path:
    """A bare git repository holding `files`, imported into graft `repo`."""
    src = d / "seed-src"
    git(["init", "-q", "-b", "main", src])
    for p, data in files.items():
        f = src / p
        f.parent.mkdir(parents=True, exist_ok=True)
        f.write_bytes(data)
    git(["add", "-A"], cwd=src)
    git(["commit", "-q", "-m", "live awareness: initial modules"], cwd=src)
    bare = d / "seed.git"
    git(["clone", "-q", "--bare", src, bare])
    shutil.rmtree(src)
    if repo:
        mirror.import_git(bare, repo, d / "seed-mirror.db")
    return bare


def _edit_disk(wc: Path, st: Step) -> None:
    p = wc / st.path
    p.write_bytes(edit_text(p.read_bytes(), st.key, st.marker))


@dataclass
class Run:
    counts: Counts = field(default_factory=Counts)
    events: list[Event] = field(default_factory=list)
    landed: list[str] = field(default_factory=list)
    tips: list[str] = field(default_factory=list)
    probe_s: list[float] = field(default_factory=list)
    errors: list[str] = field(default_factory=list)
    refusals: list[tuple[list[str], list[str]]] = field(default_factory=list)  # (paths the land named, paths that got markers)
    lock: threading.Lock = field(default_factory=threading.Lock)


def _conflict_paths(j: dict) -> list[str]:
    e = j.get("error", {})
    # A `conflicted` reply also lists the files that merged cleanly
    # (mode `merged`); only `markers` (or, refused by a land, no mode) conflict.
    return sorted(
        {c["path"] for c in e.get("conflicts", []) if "path" in c and c.get("mode") in (None, "markers")} | set(e.get("paths", []))
    )


class GraftDriver:
    def __init__(self, g: Graft, wcs: list[Path], run_: Run):
        self.g, self.wcs, self.r = g, wcs, run_

    def _resolve(self, wc: Path, paths: list[str]) -> None:
        for p in paths:
            data, b, n = resolve_text((wc / p).read_bytes())
            if not b:
                raise StepError(f"{p}: conflict reported with no markers")
            (wc / p).write_bytes(data)
            with self.r.lock:
                self.r.counts.conflict_blocks_resolved += b
                self.r.counts.conflict_lines_resolved += n
        self.g(["commit", "-m", "resolve by rule", "--author-kind", "agent"], cwd=wc)
        with self.r.lock:
            self.r.counts.commits += 1

    def act(self, tick: int, w: int, a: tuple) -> None:
        wc, kind = self.wcs[w], a[0]
        if kind == "edit":
            _edit_disk(wc, a[1])
        elif kind == "commit":
            st = a[1]
            self.g(["commit", "-m", f"live w{st.writer} step {st.index}", "--author-kind", "agent"], cwd=wc)
            with self.r.lock:
                self.r.counts.commits += 1
        elif kind == "sync":
            r, j = self.g.json(["sync"], cwd=wc, check=False)
            conflicts: list[str] = []
            if r.rc == 3 and j.get("error", {}).get("kind") == "conflicted":
                conflicts = _conflict_paths(j)
                self._resolve(wc, conflicts)
            elif r.rc != 0:
                raise StepError(f"sync exit {r.rc}: {(r.err or r.out).strip()[:300]}")
            with self.r.lock:
                self.r.counts.probe_calls += 1
                self.r.probe_s.append(r.secs)
                if conflicts:
                    self.r.counts.conflicts_caught_early += 1
                self.r.events.append(Event(tick, w, "sync", conflicts))
        elif kind == "land":
            self._land(tick, w, wc)
        else:
            raise StepError(f"policy action {kind} is not available")

    def _land(self, tick: int, w: int, wc: Path) -> None:
        sync = False
        refused: list[str] | None = None
        marked: set[str] = set()
        surfaced = False
        for _ in range(MAX_ATTEMPTS):
            r, j = self.g.json(["land", "--sync"] if sync else ["land"], cwd=wc, check=False)
            with self.r.lock:
                self.r.counts.land_attempts += 1
            kind = j.get("error", {}).get("kind")
            if r.rc == 0:
                with self.r.lock:
                    self.r.landed.append(j["landed"])
                    self.r.counts.lands += 1
                    self.r.events.append(Event(tick, w, "land", sorted(marked)))
                    if refused is not None:
                        self.r.refusals.append((refused, sorted(marked)))
                return
            if r.rc == 4:
                continue
            if r.rc == 3 and kind == "conflict":
                if refused is None:
                    refused = _conflict_paths(j)
                if not surfaced:
                    surfaced = True
                    with self.r.lock:
                        self.r.counts.conflicts_at_land += 1
                sync = True
                continue
            if r.rc == 3 and kind == "conflicted":
                paths = _conflict_paths(j)
                marked.update(paths)
                self._resolve(wc, paths)
                sync = False
                continue
            raise StepError(f"land exit {r.rc} ({kind}): {(r.err or r.out).strip()[:300]}")
        raise StepError(f"land did not succeed in {MAX_ATTEMPTS} attempts")


def _unmerged(git: Git, wc: Path) -> list[str]:
    return git(["diff", "--name-only", "--diff-filter=U"], cwd=wc, check=False).out.split()


class GitDriver:
    def __init__(self, git: Git, wcs: list[Path], run_: Run):
        self.git, self.wcs, self.r = git, wcs, run_

    def _rebase(self, wc: Path) -> list[str]:
        """fetch + rebase, resolving each stop by rule. Returns the paths
        that conflicted."""
        self.git(["fetch", "-q", "origin"], cwd=wc)
        rb = self.git(["-c", "merge.conflictStyle=diff3", "rebase", "-q", "origin/main"], cwd=wc, check=False)
        seen: list[str] = []
        for _ in range(MAX_ATTEMPTS):
            if rb.rc == 0:
                return sorted(set(seen))
            paths = _unmerged(self.git, wc)
            if not paths:
                raise StepError(f"git rebase failed: {(rb.err or rb.out).strip()[:300]}")
            seen += paths
            for p in paths:
                data, b, n = resolve_text((wc / p).read_bytes())
                (wc / p).write_bytes(data)
                self.r.counts.conflict_blocks_resolved += b
                self.r.counts.conflict_lines_resolved += n
            self.git(["add", *paths], cwd=wc)
            rb = self.git(["-c", "merge.conflictStyle=diff3", "rebase", "--continue"], cwd=wc, check=False)
        raise StepError("git rebase did not finish")

    def act(self, tick: int, w: int, a: tuple) -> None:
        wc, kind = self.wcs[w], a[0]
        if kind == "edit":
            _edit_disk(wc, a[1])
        elif kind == "commit":
            st = a[1]
            self.git(["commit", "-qam", f"live w{st.writer} step {st.index}"], cwd=wc)
            self.r.counts.commits += 1
        elif kind == "sync":
            t0 = time.perf_counter()
            conflicts = self._rebase(wc)
            self.r.probe_s.append(time.perf_counter() - t0)
            self.r.counts.probe_calls += 1
            if conflicts:
                self.r.counts.conflicts_caught_early += 1
            self.r.events.append(Event(tick, w, "sync", conflicts))
        elif kind == "land":
            first: list[str] = []
            for _ in range(MAX_ATTEMPTS):
                self.r.counts.land_attempts += 1
                if self.git(["push", "-q", "origin", "HEAD:main"], cwd=wc, check=False).rc == 0:
                    self.r.landed.append(self.git.head(wc))
                    self.r.counts.lands += 1
                    self.r.events.append(Event(tick, w, "land", first))
                    return
                c = self._rebase(wc)
                if c and not first:
                    first = c
                    self.r.counts.conflicts_at_land += 1
            raise StepError("push did not succeed")
        else:
            raise StepError(f"git has no {kind}")


# --- one policy run -------------------------------------------------------


@dataclass
class Ctx:
    graft: Graft
    mirror: Mirror
    git: Git
    work: Path
    log: callable


def run_graft_policy(ctx: Ctx, script: Script, policy: str, repo: str, d: Path, racing: bool = False) -> dict:
    d.mkdir(parents=True)
    _seed_repo(ctx.git, ctx.mirror, script.initial(), d, repo)
    wcs = []
    for i in range(script.writers):
        wc = d / f"w{i}"
        ctx.graft(["clone", repo, wc])
        wcs.append(wc)
    obs = d / "observer"
    ctx.graft(["clone", repo, obs])
    seed_id = ctx.graft.json(["log", "--limit", "1"], cwd=obs)[1]["changesets"][0]["id"]
    r = Run()
    drv = GraftDriver(ctx.graft, wcs, r)
    progs = [program(script, i, policy) for i in range(script.writers)]
    t0 = time.perf_counter()
    if racing:
        _race(drv, progs, r)
    else:
        sched = schedule(script.seed, [len(p) for p in progs])
        pc = [0] * len(progs)
        for tick, w in enumerate(sched):
            a = progs[w][pc[w]]
            pc[w] += 1
            drv.act(tick, w, a)
            if a[0] == "land":
                _, j = ctx.graft.json(["log", "--at", "main", "--limit", "1"], cwd=obs)
                r.tips.append(j["changesets"][0]["id"])
    wall = time.perf_counter() - t0
    final = d / "final"
    ctx.graft(["clone", repo, final])
    tree = read_tree(final)
    log_ids = list(reversed(ctx.graft.log_ids(final)))
    out = _verdict(script, r, tree, log_ids[1:] if log_ids and log_ids[0] == seed_id else log_ids, wall)
    out["tree"] = tree
    return out


def _race(drv: GraftDriver, progs: list[list[tuple]], r: Run) -> None:
    barrier = threading.Barrier(len(progs))

    def one(w: int) -> None:
        try:
            barrier.wait()
            for a in progs[w]:
                drv.act(-1, w, a)
        except Exception as e:  # noqa: BLE001 - every failure is reported
            with r.lock:
                r.errors.append(f"writer {w}: {type(e).__name__}: {e}"[:300])

    ts = [threading.Thread(target=one, args=(w,)) for w in range(len(progs))]
    for t in ts:
        t.start()
    for t in ts:
        t.join()


def run_git_policy(ctx: Ctx, script: Script, policy: str, d: Path) -> dict:
    d.mkdir(parents=True)
    bare = _seed_repo(ctx.git, ctx.mirror, script.initial(), d, "")
    wcs = []
    for i in range(script.writers):
        wc = d / f"w{i}"
        ctx.git(["clone", "-q", bare.as_uri(), wc])
        wcs.append(wc)
    r = Run()
    drv = GitDriver(ctx.git, wcs, r)
    progs = [program(script, i, policy) for i in range(script.writers)]
    sched = schedule(script.seed, [len(p) for p in progs])
    pc = [0] * len(progs)
    t0 = time.perf_counter()
    for tick, w in enumerate(sched):
        a = progs[w][pc[w]]
        pc[w] += 1
        drv.act(tick, w, a)
        if a[0] == "land":
            r.tips.append(ctx.git.head(bare, "main"))
    wall = time.perf_counter() - t0
    final = d / "final"
    ctx.git(["clone", "-q", bare.as_uri(), final])
    tree = read_tree(final)
    first_parent = ctx.git(["rev-list", "--first-parent", "--reverse", "main"], cwd=final).out.split()
    out = _verdict(script, r, tree, first_parent[1:], wall)
    out["tree"] = tree
    return out


def _subsequence(seq: list[str], of: list[str]) -> bool:
    pos = {x: i for i, x in enumerate(of)}
    last = -1
    for x in seq:
        if x not in pos or pos[x] <= last:
            return False
        last = pos[x]
    return True


def _verdict(script: Script, r: Run, tree: dict, line: list[str], wall: float) -> dict:
    """The run's counts and its correctness checks. `line` is the line's
    history after the seed, oldest first."""
    cmp = compare(script.expected(), tree)
    # A land puts the stack (several drafts, or one resolved draft) on the
    # line; the landed head is on it, once, in order.
    lands_ok = len(set(r.landed)) == len(r.landed) == script.writers and len(set(line)) == len(line) and _subsequence(r.landed, line)
    return {
        "counts": r.counts.as_dict(),
        "events": [e.key() for e in r.events],
        "final_tree_equals_expected": cmp["equal"],
        "mismatch": None if cmp["equal"] else cmp,
        "no_lost_or_duplicated_lands": lands_ok and not r.errors,
        "line_never_moved_backwards": _subsequence(r.tips, line) and _subsequence(r.landed, line),
        "probe_latency": summarize(r.probe_s),
        # A refused land names conflicting paths; the sync that follows
        # writes the markers. Every named path must have conflicted; a
        # refusal that names fewer is counted, not gated.
        "land_refusals": len(r.refusals),
        "land_refusals_sound": all(set(a) and set(a) <= set(b) for a, b in r.refusals),
        "land_refusals_incomplete": sum(1 for a, b in r.refusals if set(a) != set(b)),
        "wall_s": round(wall, 3),
        "errors": r.errors[:5],
    }


# --- the MCP conflict scenario ---------------------------------------------


def mcp_scenario(ctx: Ctx, url: str, token_file: Path, repo_mcp: str, repo_cli: str, d: Path) -> dict:
    """Two writers change one hot key. Writer A lands; writer B's land
    conflicts, B resolves by rule and lands. Over MCP (graft_land,
    graft_conflicts, graft_resolve, graft_land) and over the CLI; both
    must report the same conflict and end with the same file."""
    path, key = shared_path(0), "HOT_0_0"
    script = Script(0, 2, 1, "low")
    base = script.initial()
    a_text = edit_text(base[path], key, "a")
    b_text = edit_text(base[path], key, "b")
    want = render(path, script.keys[path], {key: {"a", "b"}})
    d.mkdir(parents=True)
    out: dict = {"path": path}

    # The CLI path.
    cd = d / "cli"
    cd.mkdir()
    _seed_repo(ctx.git, ctx.mirror, base, cd, repo_cli)
    A, B = cd / "a", cd / "b"
    ctx.graft(["clone", repo_cli, A])
    ctx.graft(["clone", repo_cli, B])
    (A / path).write_bytes(a_text)
    ctx.graft(["commit", "-m", "a"], cwd=A)
    ctx.graft(["land"], cwd=A)
    (B / path).write_bytes(b_text)
    ctx.graft(["commit", "-m", "b"], cwd=B)
    r, j = ctx.graft.json(["land"], cwd=B, check=False)
    cli_conflicts = _conflict_paths(j) if r.rc == 3 else []
    r, j = ctx.graft.json(["land", "--sync"], cwd=B, check=False)
    if r.rc == 3:
        (B / path).write_bytes(resolve_text((B / path).read_bytes())[0])
        ctx.graft(["commit", "-m", "resolve"], cwd=B)
        r, j = ctx.graft.json(["land"], cwd=B, check=False)
    cli_landed = r.rc == 0
    fc = cd / "final"
    ctx.graft(["clone", repo_cli, fc])
    cli_file = (fc / path).read_bytes()
    out["cli"] = {"conflicts": cli_conflicts, "landed": cli_landed, "file_equals_expected": cli_file == want}

    # The MCP path.
    md = d / "mcp"
    md.mkdir()
    _seed_repo(ctx.git, ctx.mirror, base, md, repo_mcp)
    c = Mcp(url, token_file)
    c.initialize()
    has = {t: t in c.tools for t in ("graft_land", "graft_conflicts", "graft_resolve")}
    out["mcp_tools"] = has

    def ws_open() -> str:
        err, p = c.call("graft_workspace", {"repo": repo_mcp, "action": "open", "line": "main"})
        if err:
            raise StepError(f"graft_workspace open: {p}")
        return p["workspace"]["name"]

    def commit(ws: str, data: bytes, msg: str) -> None:
        err, p = c.call(
            "graft_commit", {"repo": repo_mcp, "workspace": ws, "message": msg, "files": [{"path": path, "content": data.decode()}]}
        )
        if err:
            raise StepError(f"graft_commit: {p}")

    wa, wb = ws_open(), ws_open()
    commit(wa, a_text, "a")
    commit(wb, b_text, "b")
    err, p = c.call("graft_land", {"repo": repo_mcp, "workspace": wa})
    if err:
        raise StepError(f"graft_land a: {p}")
    calls = ["graft_land"]
    err, p = c.call("graft_land", {"repo": repo_mcp, "workspace": wb})
    calls.append("graft_land")
    mcp_conflicts = sorted(p.get("paths", [])) if err and p.get("kind") == "conflict" else []
    if has["graft_conflicts"] and has["graft_resolve"]:
        out["resolution"] = "graft_conflicts + graft_resolve"
        err, conf = c.call("graft_conflicts", {"repo": repo_mcp, "workspace": wb})
        calls.append("graft_conflicts")
        if err:
            raise StepError(f"graft_conflicts: {conf}")
        out["graft_conflicts"] = {k: v for k, v in conf.items() if k not in ("files", "content", "text")}
        resolved = want.decode()
        err, res = c.call("graft_resolve", {"repo": repo_mcp, "workspace": wb, "files": [{"path": path, "content": resolved}]})
        calls.append("graft_resolve")
        if err:
            raise StepError(f"graft_resolve: {res}")
        land_ws = wb
    else:
        # No conflict tools on this server: resolve on the client with the
        # three versions read over MCP, and land from a fresh workspace.
        out["resolution"] = "fallback: graft_read the three sides, merge and resolve locally, land from a new workspace"

        def read(at: str) -> bytes:
            err, p = c.call("graft_read", {"repo": repo_mcp, "path": path, "at": at})
            if err:
                raise StepError(f"graft_read: {p}")
            return (p.get("content") or p.get("text") or "").encode()

        theirs = read("main")
        merged, conflicted = Merger(ctx.git, md / "merge").merge(base[path], b_text, theirs)
        resolved = resolve_text(merged)[0] if conflicted else merged
        land_ws = ws_open()
        commit(land_ws, resolved, "resolve")
        calls += ["graft_read", "graft_workspace", "graft_commit"]
    err, p = c.call("graft_land", {"repo": repo_mcp, "workspace": land_ws})
    calls.append("graft_land")
    mcp_landed = not err and p.get("outcome") == "landed"
    fm = md / "final"
    ctx.graft(["clone", repo_mcp, fm])
    mcp_file = (fm / path).read_bytes()
    out["mcp"] = {"conflicts": mcp_conflicts, "landed": mcp_landed, "file_equals_expected": mcp_file == want, "calls": calls}
    out["mcp_equals_cli"] = mcp_conflicts == cli_conflicts and mcp_file == cli_file and mcp_landed == cli_landed
    out["ok"] = out["mcp_equals_cli"] and mcp_landed and cli_conflicts == [path] and mcp_file == want
    return out


# --- the whole mode ------------------------------------------------------


def available_policies(graft: Graft) -> dict[str, bool]:
    """`preview` and `drafts` switch on when `graft status --help` offers
    `--upstream` and `--drafts`."""
    r = run([graft.bin, "status", "--help"], check=False)
    text = r.out + r.err
    return {"late": True, "sync-early": True, "preview": "--upstream" in text, "drafts": "--drafts" in text}


def repo_names(prefix: str, overlaps: list[str], racing: bool) -> list[str]:
    names = []
    for o in overlaps:
        for p in GRAFT_POLICIES:
            names += [f"{prefix}-la-{o}-{p}-1", f"{prefix}-la-{o}-{p}-2"]
            if racing:
                names.append(f"{prefix}-la-{o}-{p}-race")
    return names + [f"{prefix}-la-mcp", f"{prefix}-la-mcp-cli"]


def run_mode(ctx: Ctx, site, prefix: str, seed: int, writers: int, steps: int, overlaps: list[str], racing: bool) -> dict:
    log = ctx.log
    root = ctx.work / "live-awareness"
    if root.exists():
        shutil.rmtree(root)
    root.mkdir(parents=True)
    avail = available_policies(ctx.graft)
    res: dict = {"seed": seed, "writers": writers, "steps": steps, "policies_available": avail, "levels": []}
    with tempfile.TemporaryDirectory(dir=root) as tmp:
        merger = Merger(ctx.git, Path(tmp))
        for o in overlaps:
            script = Script(seed, writers, steps, o)
            lvl: dict = {"overlap": o, "script_sha256": script.digest(), "kinds": script.kinds(), "graft": {}, "git": {}}
            log(f"[live] overlap {o}: {writers} writers x {steps} steps {script.kinds()}")
            for p in GRAFT_POLICIES:
                if not avail[p]:
                    lvl["graft"][p] = {"skipped": f"graft status --help has no --{'upstream' if p == 'preview' else 'drafts'}"}
                    continue
                runs = [run_graft_policy(ctx, script, p, f"{prefix}-la-{o}-{p}-{n}", root / f"{o}-{p}-{n}") for n in (1, 2)]
                progs = [program(script, i, p) for i in range(writers)]
                oracle = Oracle(script, p, merger).run(schedule(seed, [len(x) for x in progs]), progs)
                x = runs[0]
                x["deterministic"] = all(runs[0][k] == runs[1][k] for k in ("counts", "events", "tree"))
                x["oracle_events_match"] = x["events"] == [e.key() for e in oracle.events]
                x["oracle_counts_match"] = _oracle_counts(x["counts"]) == _oracle_counts(oracle.counts.as_dict())
                x["oracle_tree_equals_expected"] = oracle.tip == script.expected()
                x["oracle_draft_overlap"] = oracle.draft_overlap()
                x["wall_s_runs"] = [runs[0]["wall_s"], runs[1]["wall_s"]]
                if racing:
                    rr = run_graft_policy(ctx, script, p, f"{prefix}-la-{o}-{p}-race", root / f"{o}-{p}-race", racing=True)
                    x["racing"] = {
                        "final_tree_equals_expected": rr["final_tree_equals_expected"],
                        "no_lost_or_duplicated_lands": rr["no_lost_or_duplicated_lands"],
                        "too_late": rr["counts"]["conflicts_at_land"],
                        "counts": rr["counts"],
                        "wall_s": rr["wall_s"],
                        "errors": rr["errors"],
                    }
                lvl["graft"][p] = x
                log(f"[live] {o} graft {p}: {x['counts']}")
            for p in GIT_POLICIES:
                runs = [run_git_policy(ctx, script, p, root / f"{o}-git-{p}-{n}") for n in (1, 2)]
                x = runs[0]
                x["deterministic"] = all(runs[0][k] == runs[1][k] for k in ("counts", "events", "tree"))
                lvl["git"][p] = x
                log(f"[live] {o} git {p}: {x['counts']}")
            trees = [x.pop("tree") for side in ("graft", "git") for x in lvl[side].values() if "tree" in x]
            lvl["graft_equals_git"] = all(t == trees[0] for t in trees)
            for side in ("graft", "git"):
                for x in lvl[side].values():
                    x.pop("events", None)
            res["levels"].append(lvl)
    log("[live] MCP conflict scenario")
    res["mcp"] = mcp_scenario(ctx, site.url, site.token_file, f"{prefix}-la-mcp", f"{prefix}-la-mcp-cli", root / "mcp")
    res["ok"] = all_ok(res)
    return res


def _oracle_counts(c: dict) -> dict:
    keys = ("conflicts_at_land", "conflicts_caught_early", "land_attempts", "probe_calls", "commits", "lands")
    return {k: c[k] for k in keys}


def all_ok(res: dict) -> bool:
    checks = [res["mcp"]["ok"]]
    for lvl in res["levels"]:
        checks.append(lvl["graft_equals_git"])
        for side in ("graft", "git"):
            for x in lvl[side].values():
                if "skipped" in x:
                    continue
                checks += [
                    x["deterministic"],
                    x["final_tree_equals_expected"],
                    x["no_lost_or_duplicated_lands"],
                    x["line_never_moved_backwards"],
                    x["land_refusals_sound"],
                ]
                if side == "graft":
                    checks += [x["oracle_events_match"], x["oracle_counts_match"], x["oracle_tree_equals_expected"]]
                    if "racing" in x:
                        checks += [x["racing"]["final_tree_equals_expected"], x["racing"]["no_lost_or_duplicated_lands"]]
    return all(checks)
