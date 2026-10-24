"""Branching strategies through graft and git (WI-2362).

    python3 -m graftbench.branching --bin-dir ../target/release --out results/<dir>

One scripted history per strategy (trunk-based, git flow, release branches
only), generated from `--seed`, replayed through the `graft` CLI and through
`git`, and checked against a model. Measurement only (D22): nothing builds,
tests or checks code.

Every edit adds a marker to one `key = "..."` line of a module, so a line's
value only grows. A conflict is resolved by rule, the same for both systems:
each key in the block becomes `ours | (theirs - base)` (the union when the
block has no base section). With that rule a merge is the union of both
sides and a cherry-pick adds exactly the markers its changeset added, so the
model can predict every line's final tree.

graft has no merge or cherry-pick command and no merge record (a changeset
has one parent from the CLI), so moving work between lines is done the way
a client can today: `graft diff BASE SRC` for the paths, `graft cat` for
each path at both ends, a local three-way merge into the working copy
(`git merge-file`, standing in for any client-side merger), `graft commit`
and `graft land`. That "carry" is a squash merge (BASE = the last carried
point) or a cherry-pick (BASE = the changeset's parent). Two graft variants
choose BASE for a repeated merge between the same two lines:

- `tracked`: the client remembers the last source changeset it carried;
- `mergebase`: it asks the server (`graft api merge_base`), which can only
  answer the original fork point, because a carry records no second parent.

A third variant uses the server's merge (D29, WI-2368):

- `linemerge`: `graft line merge SRC --into DST` merges on the server and
  records a merge point, so the next merge starts from it. On a conflict the
  error carries the base, the source and the tip; the client carries
  base..source into a working copy on DST, resolves by rule and lands with
  `graft land --merged-from SRC@SOURCE`, which records the merge point.

Tags: graft has no tag command (tag refs come only from `graft-mirror
follow`, WI-2342), so a tag is a line `tags/<name>` made with
`graft line create --from`. It is mutable, unlike a git tag.
"""

from __future__ import annotations

import argparse
import base64
import datetime
import json
import random
import re
import shutil
import tempfile
from dataclasses import dataclass
from pathlib import Path

from .model import compare, read_tree, tree_digest
from .proc import StepError
from .site import LocalSite
from .tools import Git, Graft, Mirror

DIR = "app"
MODULES = 8
KEYS = 6
PAD = 4
VALUE_RE = re.compile(r'^([A-Za-z0-9_]+) = "([^"]*)"$')
MAX_ATTEMPTS = 12
STRATEGIES = ("trunk", "gitflow", "release")
GRAFT_VARIANTS = ("tracked", "mergebase", "linemerge")


# --------------------------------------------------------------- content


def mod_path(m: int) -> str:
    return f"{DIR}/mod_{m}.py"


def render(path: str, values: dict[str, set[str]]) -> bytes:
    out = [f'"""Branching bench module {path}."""', ""]
    for k in range(KEYS):
        key = f"k{k}"
        out.append(f'{key} = "{",".join(sorted(values.get(key, ())))}"')
        out += [f"PAD_{key}_{n} = {n}" for n in range(PAD)]
    return ("\n".join(out) + "\n").encode()


def initial() -> dict[str, dict[str, set[str]]]:
    return {mod_path(m): {f"k{k}": set() for k in range(KEYS)} for m in range(MODULES)}


def edit_text(data: bytes, key: str, marker: str) -> bytes:
    lines = data.decode().split("\n")
    for n, line in enumerate(lines):
        m = VALUE_RE.match(line)
        if m and m.group(1) == key:
            vals = {v for v in m.group(2).split(",") if v} | {marker}
            lines[n] = f'{key} = "{",".join(sorted(vals))}"'
            return "\n".join(lines).encode()
    raise StepError(f"no key {key} in the file")


def resolve_text(data: bytes) -> tuple[bytes, int]:
    """Resolves each conflict block: every key becomes ours | (theirs - base),
    or the union of the sides when the block has no base section."""
    out: list[str] = []
    blocks = 0
    sect = None
    sides: dict[str, dict[str, set[str]]] = {}
    for line in data.decode().split("\n"):
        if line.startswith("<<<<<<<"):
            sect, sides = "ours", {"ours": {}, "base": {}, "theirs": {}}
            blocks += 1
            continue
        if sect and line.startswith("|||||||"):
            sect = "base"
            continue
        if sect and line.startswith("======="):
            sect = "theirs"
            continue
        if sect and line.startswith(">>>>>>>"):
            keys = list(dict.fromkeys(list(sides["ours"]) + list(sides["theirs"])))
            for k in keys:
                o, b, t = (sides[s].get(k, set()) for s in ("ours", "base", "theirs"))
                v = o | (t - b)
                out.append(f'{k} = "{",".join(sorted(x for x in v if x))}"')
            sect = None
            continue
        if sect:
            m = VALUE_RE.match(line)
            if m:
                sides[sect].setdefault(m.group(1), set()).update(x for x in m.group(2).split(",") if x)
            continue
        out.append(line)
    if sect:
        raise StepError("an unterminated conflict block")
    return "\n".join(out).encode(), blocks


# ---------------------------------------------------------------- script


@dataclass(frozen=True)
class Edit:
    path: str
    key: str
    marker: str


@dataclass(frozen=True)
class Op:
    kind: str  # cut | feature | fix | merge | pick | tag
    line: str = ""  # the target line
    src: str = ""  # cut/merge: the source line; pick: the fix label
    name: str = ""  # feature/fix label, tag name
    commits: tuple = ()  # feature/fix: tuple of tuples of Edit
    writer: int = 0
    batch: int = -1  # features in one batch start from the same tip


def _edits(rng: random.Random, label: str, n: int, hot: list[tuple[str, str]]) -> tuple:
    out = []
    for i in range(n):
        if hot and rng.random() < 0.3:
            path, key = rng.choice(hot)
        else:
            path, key = mod_path(rng.randrange(MODULES)), f"k{rng.randrange(KEYS)}"
        out.append(Edit(path, key, f"{label}.{i}"))
    return tuple(out)


def _feature(rng, label, line, writer, batch, hot) -> Op:
    commits = tuple(_edits(rng, f"{label}c{c}", rng.randint(1, 2), hot) for c in range(rng.randint(1, 3)))
    return Op("feature", line=line, name=label, commits=commits, writer=writer, batch=batch)


def _fix(rng, label, line, hot) -> Op:
    return Op("fix", line=line, name=label, commits=(_edits(rng, label, rng.randint(1, 2), hot),))


def script(strategy: str, seed: int, writers: int = 3, rounds: int = 3) -> list[Op]:
    rng = random.Random(f"{strategy}:{seed}")
    hot = [(mod_path(0), "k0"), (mod_path(1), "k1"), (mod_path(2), "k2")]
    ops: list[Op] = []
    batch = 0

    def features(line: str, tag: str) -> None:
        nonlocal batch
        for w in range(writers):
            ops.append(_feature(rng, f"{tag}w{w}", line, w, batch, hot))
        batch += 1

    if strategy == "trunk":
        for r in range(rounds * 2):
            features("main", f"t{r}")
            if r % 2 == 1:
                ops.append(Op("tag", line="main", name=f"v0.{r // 2}"))
        return ops
    if strategy == "gitflow":
        ops.append(Op("cut", line="develop", src="main"))
        for rel in range(2):
            for r in range(rounds):
                features("develop", f"d{rel}r{r}")
            rl = f"release/1.{rel}"
            ops.append(Op("cut", line=rl, src="develop"))
            features("develop", f"d{rel}next")  # develop moves on while the release stabilises
            ops.append(_fix(rng, f"rf{rel}a", rl, hot))
            ops.append(_fix(rng, f"rf{rel}b", rl, hot))
            ops.append(Op("merge", line="develop", src=rl))  # interim back-merge
            features("develop", f"d{rel}mid")
            ops.append(_fix(rng, f"rf{rel}c", rl, hot))
            ops.append(Op("merge", line="main", src=rl))
            ops.append(Op("tag", line="main", name=f"v1.{rel}.0"))
            ops.append(Op("merge", line="develop", src=rl))
            features("develop", f"d{rel}post")
            # hotfix/1.x.1: from main, merged to main and develop.
            ops.append(_fix(rng, f"hf{rel}", "main", hot))
            ops.append(Op("tag", line="main", name=f"v1.{rel}.1"))
            ops.append(Op("pick", line="develop", src=f"hf{rel}"))
        return ops
    if strategy == "release":
        for rel in range(2):
            for r in range(rounds):
                features("main", f"m{rel}r{r}")
            rl = f"release/2.{rel}"
            ops.append(Op("cut", line=rl, src="main"))
            ops.append(Op("tag", line=rl, name=f"v2.{rel}.0"))
            features("main", f"m{rel}next")
            ops.append(_fix(rng, f"mf{rel}", "main", hot))  # fixed on main, picked to the release
            ops.append(Op("pick", line=rl, src=f"mf{rel}"))
            ops.append(_fix(rng, f"rf{rel}", rl, hot))  # release-only fix
            ops.append(Op("tag", line=rl, name=f"v2.{rel}.1"))
        # one late fix picked into both release lines
        ops.append(_fix(rng, "late", "main", hot))
        ops.append(Op("pick", line="release/2.0", src="late"))
        ops.append(Op("pick", line="release/2.1", src="late"))
        ops.append(Op("tag", line="release/2.0", name="v2.0.2"))
        ops.append(Op("tag", line="release/2.1", name="v2.1.2"))
        return ops
    raise ValueError(strategy)


def model(ops: list[Op]) -> tuple[dict[str, dict], dict[str, dict]]:
    """Expected marker sets per line and per tag."""
    lines = {"main": initial()}
    tags: dict[str, dict] = {}
    added: dict[str, list[Edit]] = {}

    def copy(s):
        return {p: {k: set(v) for k, v in ks.items()} for p, ks in s.items()}

    for op in ops:
        if op.kind == "cut":
            lines[op.line] = copy(lines[op.src])
        elif op.kind in ("feature", "fix"):
            for c in op.commits:
                for e in c:
                    lines[op.line][e.path][e.key].add(e.marker)
            added[op.name] = [e for c in op.commits for e in c]
        elif op.kind == "merge":
            for p, ks in lines[op.src].items():
                for k, v in ks.items():
                    lines[op.line][p][k] |= v
        elif op.kind == "pick":
            for e in added[op.src]:
                lines[op.line][e.path][e.key].add(e.marker)
        elif op.kind == "tag":
            tags[op.name] = copy(lines[op.line])
    return lines, tags


def as_tree(state: dict) -> dict[str, bytes]:
    return {p: render(p, ks) for p, ks in state.items()}


# ---------------------------------------------------------------- counting


@dataclass
class Counts:
    commands: int = 0  # every CLI process
    round_trips: int = 0  # commands that contact the server / remote
    conflicts: int = 0  # operations that hit at least one conflict
    conflict_blocks: int = 0
    land_retries: int = 0  # graft: refused land then --sync; git: rejected push
    line_merged_paths: int = 0  # graft: paths the land's rebase merged by line
    carries: int = 0
    carry_clean: int = 0
    carry_conflicted: int = 0

    def d(self) -> dict:
        return dict(self.__dict__)


# ---------------------------------------------------------------- graft


class GraftRun:
    def __init__(self, g: Graft, repo: str, work: Path, git: Git, variant: str):
        self.g, self.repo, self.w, self.git, self.variant = g, repo, work, git, variant
        self.c = Counts()
        self.wcs: dict[tuple, Path] = {}
        self.fixes: dict[str, str] = {}  # label -> landed changeset
        self.carried: dict[tuple, str] = {}  # (src, dst) -> last src changeset carried
        self.tags: dict[str, str] = {}
        self.notes: list[str] = []
        self.lines: set[str] = {"main"}
        self.oplog: list[dict] = []

    def call(self, args, cwd=None, check=True, rt=True):
        self.c.commands += 1
        self.c.round_trips += int(rt)
        return self.g(args, cwd=cwd, check=check)

    def jcall(self, args, cwd=None, check=True, rt=True, input=None):
        self.c.commands += 1
        self.c.round_trips += int(rt)
        r = self.g(args + ["--json"], cwd=cwd, check=check, input=input)
        try:
            return r, json.loads(r.out or r.err or "{}")
        except json.JSONDecodeError:
            return r, {}

    def wc(self, who, line: str) -> Path:
        k = (who, line)
        if k not in self.wcs:
            d = self.w / f"g-{who}-{line.replace('/', '_')}"
            self.call(["clone", self.repo, d, "--line", line])
            self.wcs[k] = d
        else:
            self.call(["sync"], cwd=self.wcs[k])
        return self.wcs[k]

    def tip(self, ref: str) -> str:
        _, j = self.jcall(["log", "--at", ref, "--limit", "1", "--repo", self.repo], cwd=self.w)
        return j["changesets"][0]["id"]

    def resolve_files(self, wc: Path, paths) -> int:
        blocks = 0
        for p in paths:
            data, b = resolve_text((wc / p).read_bytes())
            (wc / p).write_bytes(data)
            blocks += b
        return blocks

    def land(self, wc: Path, extra: list[str] | None = None) -> str:
        sync = False
        hit = False
        for _ in range(MAX_ATTEMPTS):
            args = (["land", "--sync"] if sync else ["land"]) + (extra or [])
            r, j = self.jcall(args, cwd=wc, check=False)
            kind = j.get("error", {}).get("kind")
            if r.rc == 0:
                self.c.line_merged_paths += len(j.get("preflight", {}).get("merges_cleanly", []) or [])
                return j["landed"]
            if r.rc == 4:
                continue
            if r.rc == 3 and kind == "conflicted":
                e = j.get("error", {})
                paths = sorted({c["path"] for c in e.get("conflicts", []) if c.get("mode") in (None, "markers")} | set(e.get("paths", [])))
                if not hit:
                    hit = True
                    self.c.conflicts += 1
                self.c.conflict_blocks += self.resolve_files(wc, paths)
                self.call(["commit", "-m", "resolve by rule"], cwd=wc)
                sync = False
                continue
            if r.rc == 3:
                self.c.land_retries += 1
                sync = True
                continue
            raise StepError(f"graft land exit {r.rc} ({kind}): {(r.err or r.out).strip()[:300]}")
        raise StepError(f"graft land did not succeed in {MAX_ATTEMPTS} attempts")

    def edit_commit(self, wc: Path, commits, label: str) -> None:
        for i, c in enumerate(commits):
            for e in c:
                p = wc / e.path
                p.write_bytes(edit_text(p.read_bytes(), e.key, e.marker))
            self.call(["commit", "-m", f"{label} {i}"], cwd=wc)

    def cut(self, op: Op) -> None:
        self.call(["line", "create", op.line, "--from", op.src, "--repo", self.repo], cwd=self.w)
        self.lines.add(op.line)

    def batch(self, ops: list[Op]) -> None:
        wcs = [self.wc(f"w{op.writer}", op.line) for op in ops]
        for op, wc in zip(ops, wcs):
            self.edit_commit(wc, op.commits, op.name)
        for wc in wcs:
            self.land(wc)

    def fix(self, op: Op) -> None:
        wc = self.wc("int", op.line)
        self.edit_commit(wc, op.commits, op.name)
        self.fixes[op.name] = self.land(wc)

    def cat(self, path: str, at: str) -> bytes | None:
        r, j = self.jcall(["cat", path, "--at", at, "--repo", self.repo], cwd=self.w, check=False)
        if r.rc != 0:
            return None
        return base64.b64decode(j["content"]) if j.get("encoding") == "base64" else j["content"].encode()

    def carry(self, base: str, src: str, dst: str, land_extra: list[str] | None = None) -> None:
        """Applies base..src onto dst's tip in a working copy and lands it."""
        self.c.carries += 1
        wc = self.wc("int", dst)
        files, cursor = [], None
        while True:
            args = ["diff", base, src, "--repo", self.repo] + (["--cursor", cursor] if cursor else [])
            _, j = self.jcall(args, cwd=self.w)
            files += j.get("files", [])
            cursor = j.get("next")
            if not cursor:
                break
        conflicted = 0
        for f in files:
            p = f["path"]
            b, t = self.cat(p, base) or b"", self.cat(p, src)
            target = wc / p
            o = target.read_bytes() if target.exists() else b""
            if t is None:
                if o == b and target.exists():
                    target.unlink()
                continue
            merged, clash = merge3(self.git, self.w, o, b, t)
            if clash:
                conflicted += 1
                merged, blocks = resolve_text(merged)
                self.c.conflict_blocks += blocks
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(merged)
        if conflicted:
            self.c.conflicts += 1
            self.c.carry_conflicted += 1
        else:
            self.c.carry_clean += 1
        st = self.g(["status", "--json"], cwd=wc)
        self.c.commands += 1
        if not json.loads(st.out).get("clean", True):
            self.call(["commit", "-m", f"carry {src[:12]} onto {dst}"], cwd=wc)
            self.land(wc, land_extra)
        else:
            self.notes.append(f"carry onto {dst}: nothing to land")

    def line_merge(self, op: Op) -> None:
        """`graft line merge`; on a conflict, carry from the server's base and
        land the resolution with `--merged-from`."""
        for _ in range(MAX_ATTEMPTS):
            args = ["line", "merge", op.src, "--into", op.line, "--repo", self.repo]
            r, j = self.jcall(args, cwd=self.w, check=False)
            if r.rc == 0:
                self.c.carries += 1
                self.c.carry_clean += 1
                self.c.line_merged_paths += len(j.get("merged_paths", []) or [])
                if j.get("via") == "ancestry":
                    self.notes.append(f"merge {op.src} -> {op.line}: base via ancestry")
                return
            if r.rc == 4:
                continue
            e = j.get("error", {})
            if r.rc == 3 and e.get("kind") == "conflict" and e.get("base") and e.get("source"):
                self.carry(e["base"], e["source"], op.line, ["--merged-from", f"{op.src}@{e['source']}"])
                return
            raise StepError(f"graft line merge exit {r.rc} ({e.get('kind')}): {(r.err or r.out).strip()[:300]}")
        raise StepError(f"graft line merge did not succeed in {MAX_ATTEMPTS} attempts")

    def merge(self, op: Op) -> None:
        if self.variant == "linemerge":
            self.line_merge(op)
            return
        src_tip = self.tip(op.src)
        if self.variant == "tracked" and (op.src, op.line) in self.carried:
            base = self.carried[(op.src, op.line)]
        else:
            _, j = self.jcall(
                ["api", "merge_base", "--repo", self.repo], cwd=self.w, input=json.dumps({"a": op.line, "b": op.src}).encode()
            )
            base = j["merge_base"]
        self.carry(base, src_tip, op.line)
        self.carried[(op.src, op.line)] = src_tip

    def pick(self, op: Op) -> None:
        cs = self.fixes[op.src]
        _, j = self.jcall(["show", cs, "--repo", self.repo], cwd=self.w)
        self.carry(j["parents"][0], cs, op.line)

    def tag(self, op: Op) -> None:
        name = f"tags/{op.name}"
        self.call(["line", "create", name, "--from", op.line, "--repo", self.repo], cwd=self.w)
        self.tags[op.name] = name

    def tree(self, ref: str) -> dict[str, bytes]:
        d = self.w / f"final-{ref.replace('/', '_')}"
        self.g(["clone", self.repo, d, "--at", ref])
        return read_tree(d)


def merge3(git: Git, d: Path, ours: bytes, base: bytes, theirs: bytes) -> tuple[bytes, bool]:
    paths = []
    for name, data in (("m-ours", ours), ("m-base", base), ("m-theirs", theirs)):
        p = d / name
        p.write_bytes(data)
        paths.append(p)
    r = git(["merge-file", "-p", "--diff3", *paths], check=False)
    if r.rc < 0 or r.rc > 127:
        raise StepError(f"git merge-file failed: {r.err.strip()[:300]}")
    return r.out.encode(), r.rc > 0


# ---------------------------------------------------------------- git


class GitRun:
    def __init__(self, git: Git, bare: Path, work: Path, strategy: str):
        self.git, self.bare, self.w, self.strategy = git, bare, work, strategy
        self.c = Counts()
        self.clones: dict[str, Path] = {}
        self.fixes: dict[str, str] = {}
        self.oplog: list[dict] = []

    REMOTE = {"clone", "fetch", "push", "pull"}

    def call(self, args, cwd=None, check=True):
        self.c.commands += 1
        self.c.round_trips += int(args[0] in self.REMOTE)
        return self.git(["-c", "merge.conflictStyle=diff3"] + args, cwd=cwd, check=check)

    def clone(self, who: str) -> Path:
        if who not in self.clones:
            d = self.w / f"git-{who}"
            self.call(["clone", "-q", self.bare, d])
            self.clones[who] = d
        else:
            self.call(["fetch", "-q", "--prune", "origin"], cwd=self.clones[who])
        return self.clones[who]

    def unmerged(self, wc: Path) -> list[str]:
        self.c.commands += 1
        return self.git(["diff", "--name-only", "--diff-filter=U"], cwd=wc, check=False).out.split()

    def resolve(self, wc: Path) -> int:
        paths = self.unmerged(wc)
        blocks = 0
        for p in paths:
            data, b = resolve_text((wc / p).read_bytes())
            (wc / p).write_bytes(data)
            blocks += b
        self.call(["add", "-A"], cwd=wc)
        return blocks

    def edit_commit(self, wc: Path, commits, label: str) -> None:
        for i, c in enumerate(commits):
            for e in c:
                p = wc / e.path
                p.write_bytes(edit_text(p.read_bytes(), e.key, e.marker))
            self.call(["commit", "-qam", f"{label} {i}"], cwd=wc)

    def _rebase_push(self, wc: Path, line: str) -> str:
        hit = False
        for _ in range(MAX_ATTEMPTS):
            r = self.call(["rebase", f"origin/{line}"], cwd=wc, check=False)
            while r.rc != 0:
                if not self.unmerged(wc):
                    raise StepError(f"git rebase failed: {r.err.strip()[:300]}")
                if not hit:
                    hit = True
                    self.c.conflicts += 1
                self.c.conflict_blocks += self.resolve(wc)
                r = self.call(["-c", "core.editor=true", "rebase", "--continue"], cwd=wc, check=False)
            if self.call(["push", "-q", "origin", f"HEAD:{line}"], cwd=wc, check=False).rc == 0:
                return self.git(["rev-parse", "HEAD"], cwd=wc).out.strip()
            self.c.land_retries += 1
            self.call(["fetch", "-q", "origin"], cwd=wc)
        raise StepError("git push did not succeed")

    def _merge_push(self, wc: Path, line: str, what: str, msg: str, pick: bool = False) -> str:
        hit = False
        for _ in range(MAX_ATTEMPTS):
            self.call(["checkout", "-q", "-B", line, f"origin/{line}"], cwd=wc)
            args = ["cherry-pick", "-x", what] if pick else ["merge", "--no-ff", "-m", msg, what]
            r = self.call(args, cwd=wc, check=False)
            if r.rc != 0:
                if not self.unmerged(wc):
                    raise StepError(f"git {args[0]} failed: {r.err.strip()[:300]}")
                if not hit:
                    hit = True
                    self.c.conflicts += 1
                self.c.conflict_blocks += self.resolve(wc)
                self.call(["-c", "core.editor=true", "cherry-pick" if pick else "commit", "--continue" if pick else "--no-edit"], cwd=wc)
            if self.call(["push", "-q", "origin", line], cwd=wc, check=False).rc == 0:
                return self.git(["rev-parse", "HEAD"], cwd=wc).out.strip()
            self.c.land_retries += 1
            self.call(["fetch", "-q", "origin"], cwd=wc)
        raise StepError("git push did not succeed")

    def cut(self, op: Op) -> None:
        wc = self.clone("int")
        self.call(["push", "-q", "origin", f"origin/{op.src}:refs/heads/{op.line}"], cwd=wc)

    def batch(self, ops: list[Op]) -> None:
        wcs = []
        for op in ops:
            wc = self.clone(f"w{op.writer}")
            self.call(["checkout", "-q", "-B", f"feature/{op.name}", f"origin/{op.line}"], cwd=wc)
            self.edit_commit(wc, op.commits, op.name)
            wcs.append(wc)
        for op, wc in zip(ops, wcs):
            if self.strategy == "gitflow":
                self.call(["fetch", "-q", "origin"], cwd=wc)
                self._merge_push(wc, op.line, f"feature/{op.name}", f"Merge feature/{op.name}")
            else:
                self.call(["fetch", "-q", "origin"], cwd=wc)
                self._rebase_push(wc, op.line)

    def fix(self, op: Op) -> None:
        wc = self.clone("int")
        if self.strategy == "gitflow" and op.name.startswith("hf"):
            # hotfix/*: a branch from main, merged into main (develop by its pick op)
            self.call(["checkout", "-q", "-B", f"hotfix/{op.name}", "origin/main"], cwd=wc)
            self.edit_commit(wc, op.commits, op.name)
            self._merge_push(wc, "main", f"hotfix/{op.name}", f"Merge hotfix/{op.name}")
            self.fixes[op.name] = f"hotfix/{op.name}"
            return
        self.call(["checkout", "-q", "-B", f"fix/{op.name}", f"origin/{op.line}"], cwd=wc)
        self.edit_commit(wc, op.commits, op.name)
        self.fixes[op.name] = self._rebase_push(wc, op.line)

    def merge(self, op: Op) -> None:
        wc = self.clone("int")
        self._merge_push(wc, op.line, f"origin/{op.src}", f"Merge {op.src} into {op.line}")

    def pick(self, op: Op) -> None:
        wc = self.clone("int")
        what = self.fixes[op.src]
        if what.startswith("hotfix/"):  # git flow merges the hotfix branch itself
            self._merge_push(wc, op.line, what, f"Merge {what} into {op.line}")
        else:
            self._merge_push(wc, op.line, what, "", pick=True)

    def tag(self, op: Op) -> None:
        wc = self.clone("int")
        self.call(["tag", op.name, f"origin/{op.line}"], cwd=wc)
        self.call(["push", "-q", "origin", f"refs/tags/{op.name}"], cwd=wc)

    def tree(self, ref: str) -> dict[str, bytes]:
        d = self.w / f"final-{ref.replace('/', '_')}"
        self.git(["clone", "-q", "--branch", ref, self.bare, d])
        return read_tree(d)


# ---------------------------------------------------------------- driver


def drive(runner, ops: list[Op]) -> None:
    i = 0
    while i < len(ops):
        op = ops[i]
        if op.kind == "feature":
            j = i
            while j < len(ops) and ops[j].kind == "feature" and ops[j].batch == op.batch:
                j += 1
            runner.batch(ops[i:j])
            i = j
            continue
        before = (runner.c.conflicts, runner.c.conflict_blocks, runner.c.round_trips)
        getattr(runner, op.kind)(op)
        if op.kind in ("merge", "pick"):
            runner.oplog.append(
                {
                    "op": i,
                    "kind": op.kind,
                    "src": op.src,
                    "dst": op.line,
                    "conflict": runner.c.conflicts > before[0],
                    "blocks": runner.c.conflict_blocks - before[1],
                    "round_trips": runner.c.round_trips - before[2],
                }
            )
        i += 1


def seed_repo(git: Git, d: Path) -> Path:
    src = d / "seed-src"
    git(["init", "-q", "-b", "main", src])
    for p, data in as_tree(initial()).items():
        f = src / p
        f.parent.mkdir(parents=True, exist_ok=True)
        f.write_bytes(data)
    git(["add", "-A"], cwd=src)
    git(["commit", "-q", "-m", "branching bench: initial modules"], cwd=src)
    bare = d / "seed.git"
    git(["clone", "-q", "--bare", src, bare])
    shutil.rmtree(src)
    return bare


def run_strategy(strategy: str, seed: int, site: LocalSite, bins: Path, git: Git, work: Path) -> dict:
    ops = script(strategy, seed)
    exp_lines, exp_tags = model(ops)
    d = work / strategy
    d.mkdir(parents=True)
    seed_bare = seed_repo(git, d)
    res: dict = {"strategy": strategy, "ops": _op_counts(ops), "systems": {}}

    # git
    gbare = d / "git-remote.git"
    git(["clone", "-q", "--bare", seed_bare, gbare])
    gw = d / "git"
    gw.mkdir()
    gr = GitRun(git, gbare, gw, strategy)
    drive(gr, ops)
    git_trees = {ln: gr.tree(ln) for ln in exp_lines}
    git_tags = {t: gr.tree(t) for t in exp_tags}
    res["systems"]["git"] = _verify(gr.c, git_trees, git_tags, exp_lines, exp_tags)
    res["systems"]["git"]["history"] = _git_shape(git, gbare, list(exp_lines))
    res["systems"]["git"]["oplog"] = gr.oplog

    for variant in GRAFT_VARIANTS:
        repo = f"{strategy}-{variant}"
        Mirror(bins / "graft-mirror", site.url, site.token_file).import_git(seed_bare, repo, d / f"{repo}-mirror.db")
        gd = d / f"graft-{variant}"
        gd.mkdir()
        g = Graft(bins / "graft", site.url, site.token_file)
        r = GraftRun(g, repo, gd, git, variant)
        drive(r, ops)
        trees = {ln: r.tree(ln) for ln in exp_lines}
        tags = {t: r.tree(r.tags[t]) for t in exp_tags}
        out = _verify(r.c, trees, tags, exp_lines, exp_tags)
        out["equals_git"] = all(trees[ln] == git_trees[ln] for ln in exp_lines) and all(tags[t] == git_tags[t] for t in exp_tags)
        out["notes"] = r.notes
        out["oplog"] = r.oplog
        res["systems"][f"graft-{variant}"] = out
    return res


def _op_counts(ops) -> dict:
    out: dict = {}
    for op in ops:
        out[op.kind] = out.get(op.kind, 0) + 1
    out["feature_commits"] = sum(len(op.commits) for op in ops if op.kind == "feature")
    return out


def _verify(c: Counts, trees, tags, exp_lines, exp_tags) -> dict:
    lines = {ln: compare(as_tree(exp_lines[ln]), trees[ln]) for ln in exp_lines}
    tg = {t: compare(as_tree(exp_tags[t]), tags[t]) for t in exp_tags}
    return {
        "counts": c.d(),
        "lines": {ln: {"equal": v["equal"], "digest": tree_digest(trees[ln])[:16]} for ln, v in lines.items()},
        "tags": {t: v["equal"] for t, v in tg.items()},
        "model_ok": all(v["equal"] for v in lines.values()) and all(v["equal"] for v in tg.values()),
        "diffs": {k: v for k, v in list(lines.items()) + list(tg.items()) if not v["equal"]},
    }


def _git_shape(git: Git, bare: Path, lines: list[str]) -> dict:
    out = {}
    for ln in lines:
        n = git(["rev-list", "--count", ln], cwd=bare).out.strip()
        m = git(["rev-list", "--count", "--merges", ln], cwd=bare).out.strip()
        out[ln] = {"commits": int(n), "merge_commits": int(m)}
    return out


def summary_md(results: list[dict], meta: dict) -> str:
    out = [f"# Branching strategies through graft and git ({meta['date']})", ""]
    out += [f"Seed {meta['seed']}, graft {meta['graft_version']}, {meta['git_version']}. Measurement only (D22).", ""]
    cols = [
        "commands",
        "round_trips",
        "conflicts",
        "conflict_blocks",
        "land_retries",
        "line_merged_paths",
        "carries",
        "carry_conflicted",
    ]
    for r in results:
        out += [f"## {r['strategy']}", "", "Ops: " + ", ".join(f"{k} {v}" for k, v in r["ops"].items()), ""]
        out.append("| system | model ok | = git | " + " | ".join(cols) + " |")
        out.append("|---" * (len(cols) + 3) + "|")
        for name, s in r["systems"].items():
            c = s["counts"]
            eq = s.get("equals_git", "—")
            out.append(f"| {name} | {s['model_ok']} | {eq} | " + " | ".join(str(c[k]) for k in cols) + " |")
        out.append("")
        names = list(r["systems"])
        logs = [r["systems"][n]["oplog"] for n in names]
        if logs[0]:
            out.append("Between lines (conflict blocks, round trips):")
            out.append("")
            out.append("| op | " + " | ".join(names) + " |")
            out.append("|---" * (len(names) + 1) + "|")
            for row in zip(*logs):
                o = row[0]
                cells = [f"{'conflict ' if x['conflict'] else 'clean '}{x['blocks']}b, {x['round_trips']} rt" for x in row]
                out.append(f"| {o['kind']} {o['src']} → {o['dst']} | " + " | ".join(cells) + " |")
            out.append("")
        gh = r["systems"]["git"]["history"]
        out.append("git history: " + ", ".join(f"{ln} {v['commits']} commits ({v['merge_commits']} merges)" for ln, v in gh.items()))
        out.append("")
    return "\n".join(out) + "\n"


def main(argv=None) -> int:
    p = argparse.ArgumentParser(prog="graftbench.branching", description=__doc__.split("\n\n")[0])
    p.add_argument("--bin-dir", type=Path, default=Path(__file__).resolve().parents[2] / "target" / "release")
    p.add_argument("--seed", type=int, default=2362)
    p.add_argument("--strategies", default=",".join(STRATEGIES))
    p.add_argument("--out", type=Path)
    p.add_argument("--keep", action="store_true")
    a = p.parse_args(argv)
    a.bin_dir = a.bin_dir.resolve()
    started = datetime.datetime.now(datetime.timezone.utc)
    out = a.out or Path("results") / f"{started:%Y-%m-%d}-branching"
    out.mkdir(parents=True, exist_ok=True)
    work = Path(tempfile.mkdtemp(prefix="graft-branching-"))
    git = Git()
    strategies = a.strategies.split(",")
    repos = [f"{s}-{v}" for s in strategies for v in GRAFT_VARIANTS]
    site = LocalSite(a.bin_dir / "graft-server", work / "site", repos)
    results = []
    try:
        for s in strategies:
            print(f"[branching] {s}", flush=True)
            results.append(run_strategy(s, a.seed, site, a.bin_dir, git, work))
    finally:
        site.stop()
        if not a.keep:
            shutil.rmtree(work, ignore_errors=True)
    meta = {
        "date": f"{started:%Y-%m-%d}",
        "seed": a.seed,
        "graft_version": Graft(a.bin_dir / "graft", "", Path()).version(),
        "git_version": git.version(),
    }
    (out / "results.json").write_text(json.dumps({"meta": meta, "results": results}, indent=2, sort_keys=True) + "\n")
    (out / "summary.md").write_text(summary_md(results, meta))
    print((out / "summary.md").read_text())
    ok = all(s["model_ok"] for r in results for s in r["systems"].values())
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
