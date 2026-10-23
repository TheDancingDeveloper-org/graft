"""Deterministic synthetic Python project and commit stream.

`stream(seed, n)` yields commit 0 (the initial project) and then `n`
commits of realistic edits. The same seed and `n` always give the same
commits, byte for byte, on any machine: the only source of randomness is one
`random.Random(seed)`, and every choice is made from a sorted list.

A commit is a list of operations applied in order:

- ("w", path, data)  write a file (create or replace)
- ("d", path)        delete a file
- ("r", src, dst)    rename a file (a later "w" may change its content)

Commit kinds (weights in `KINDS`): edit a function, add a function, edit
several modules, add, delete or rename a module, touch a shared file (the
registry, a package `__all__`, the changelog), rewrite part of a large data
file, add or drop a directory of many small files.
"""

from __future__ import annotations

import bisect
import hashlib
import random
from dataclasses import dataclass, field

PROJECT = "synthapp"
SRC = f"src/{PROJECT}"
REGISTRY = f"{SRC}/registry.py"
CHANGELOG = "CHANGELOG.md"

# fmt: off
WORDS = [
    "account", "action", "adapter", "agent", "alert", "archive", "asset", "audit",
    "backup", "batch", "bucket", "buffer", "cache", "channel", "check", "chunk",
    "client", "cluster", "codec", "column", "config", "cursor", "daemon", "delta",
    "digest", "domain", "draft", "driver", "entry", "event", "export", "feature",
    "filter", "flag", "format", "frame", "gateway", "graph", "group", "handle",
    "header", "index", "inode", "job", "journal", "kernel", "key", "label",
    "layer", "ledger", "lease", "limit", "line", "loader", "lock", "manifest",
    "matrix", "merge", "message", "meter", "mirror", "model", "module", "monitor",
    "node", "notice", "object", "offset", "order", "packet", "page", "parser",
    "patch", "path", "peer", "pipe", "policy", "pool", "probe", "queue",
    "quota", "range", "reader", "record", "region", "relay", "report", "request",
    "route", "rule", "sample", "schema", "scope", "segment", "sender", "server",
    "session", "shard", "signal", "slice", "snapshot", "socket", "source", "span",
    "stage", "state", "stream", "subject", "table", "task", "tenant", "ticket",
    "timer", "token", "topic", "trace", "tree", "unit", "update", "upload",
    "user", "value", "vector", "version", "view", "volume", "watch", "worker",
    "writer", "zone",
]
VERBS = [
    "apply", "build", "check", "clean", "compute", "convert", "count", "decode",
    "emit", "encode", "fetch", "find", "flush", "format", "load", "lookup",
    "make", "merge", "normalize", "parse", "prepare", "read", "render", "reset",
    "resolve", "scan", "score", "select", "split", "store", "sync", "update",
    "validate", "write",
]
IMPORTS = [
    "import math", "import os", "import re", "import json", "import time",
    "import itertools", "import functools", "import collections", "import hashlib",
    "from typing import Any", "from pathlib import Path", "from dataclasses import dataclass",
]
AUTHORS = [
    ("Ada Agent", "ada@agents.example"), ("Bo Builder", "bo@agents.example"),
    ("Cy Coder", "cy@agents.example"), ("Di Dev", "di@agents.example"),
    ("Ed Editor", "ed@agents.example"), ("Flo Fixer", "flo@agents.example"),
]
OPS = ["+", "-", "*", "^", "|", "&"]
KINDS = [
    ("edit_fn", 34), ("add_fn", 14), ("multi", 8), ("shared", 13),
    ("add_module", 9), ("delete_module", 3), ("rename_module", 4),
    ("large", 2), ("small_many", 2), ("drop_small", 1), ("edit_tail", 10),
]
# fmt: on

# Commit timestamps: a fixed epoch plus one minute per commit, so git
# commit ids are reproducible.
EPOCH = 1_760_000_000

MAX_LARGE = 4
CHANGELOG_RELEASE_AT = 400


def modules_for(n: int) -> int:
    """Initial module count for a stream of `n` commits: 200 to 2,000."""
    return max(200, min(2000, n // 50))


@dataclass
class Function:
    name: str
    args: list[str]
    doc: str
    body: list[str]  # statements, without indentation
    ret: str

    def render(self) -> str:
        lines = [f"def {self.name}({', '.join(self.args)}):", f'    """{self.doc}"""']
        lines += ["    " + s for s in self.body]
        lines.append(f"    return {self.ret}")
        return "\n".join(lines) + "\n"


@dataclass
class Module:
    doc: str
    imports: list[str]
    consts: list[tuple[str, int]]
    funcs: list[Function]
    tail: list[str] = field(default_factory=list)

    def render(self) -> bytes:
        out = [f'"""{self.doc}"""', "", "from __future__ import annotations", ""]
        out += self.imports
        out.append("")
        for name, value in self.consts:
            out.append(f"{name} = {value}")
        text = "\n".join(out) + "\n"
        for f in self.funcs:
            text += "\n\n" + f.render()
        if self.tail:
            text += "\n\n" + "\n".join(self.tail) + "\n"
        return text.encode()


@dataclass
class Commit:
    index: int
    kind: str
    message: str
    author: tuple[str, str]
    timestamp: int
    ops: list[tuple]


class Generator:
    """Holds the project state; `next_commit` advances it by one commit."""

    def __init__(self, seed: int, n: int, modules: int | None = None):
        self.rng = random.Random(seed)
        self.seed = seed
        self.n = n
        self.target_modules = modules or modules_for(n)
        self.max_modules = self.target_modules * 2
        self.files: dict[str, bytes] = {}
        self.mods: dict[str, Module] = {}
        self.mod_paths: list[str] = []  # sorted
        self.packages: list[str] = []  # sorted, package dirs
        self.pkg_all: dict[str, list[str]] = {}
        self.registry: list[str] = []  # sorted entries
        self.changelog: list[str] = []
        self.release = 0
        self.large: dict[str, list[str]] = {}
        self.small_dirs: list[str] = []
        self.counter = 0
        self.index = 0
        self.changed = False

    # -- names -------------------------------------------------------------

    def word(self) -> str:
        return self.rng.choice(WORDS)

    def uniq(self) -> int:
        self.counter += 1
        return self.counter

    def ident(self) -> str:
        return f"{self.rng.choice(VERBS)}_{self.word()}_{self.uniq()}"

    # -- content -----------------------------------------------------------

    def statement(self, names: list[str], new: str) -> str:
        r = self.rng.random()
        a = self.rng.choice(names)
        if r < 0.6:
            return f"{new} = {a} {self.rng.choice(OPS)} {self.rng.randrange(1, 997)}"
        if r < 0.8:
            b = self.rng.choice(names)
            return f"{new} = ({a} {self.rng.choice(OPS)} {b}) % {self.rng.randrange(2, 4099)}"
        return f"{new} = max({a}, {self.rng.randrange(0, 10_000)})"

    def function(self) -> Function:
        args = [self.word()[:6] + "_" + c for c in "xyz"[: self.rng.randrange(1, 4)]]
        names = list(args)
        body = []
        for i in range(self.rng.randrange(3, 12)):
            new = f"v{i}"
            body.append(self.statement(names, new))
            names.append(new)
        doc = f"{self.rng.choice(VERBS).capitalize()} the {self.word()} {self.word()}."
        return Function(self.ident(), args, doc, body, names[-1])

    def module(self) -> Module:
        imports = sorted(self.rng.sample(IMPORTS, self.rng.randrange(1, 5)))
        consts = [(f"{self.word().upper()}_{i}", self.rng.randrange(0, 100_000)) for i in range(self.rng.randrange(0, 4))]
        funcs = [self.function() for _ in range(self.rng.randrange(2, 9))]
        doc = f"The {self.word()} {self.word()} module."
        return Module(doc, imports, consts, funcs)

    # -- file bookkeeping --------------------------------------------------

    def put(self, ops: list, path: str, data: bytes) -> None:
        if self.files.get(path) != data:
            self.changed = True
        self.files[path] = data
        ops.append(("w", path, data))

    def put_mod(self, ops: list, path: str) -> None:
        if path not in self.mods:
            raise KeyError(path)
        self.put(ops, path, self.mods[path].render())

    def add_mod_path(self, path: str, mod: Module) -> None:
        self.mods[path] = mod
        bisect.insort(self.mod_paths, path)

    def drop_mod_path(self, path: str) -> None:
        del self.mods[path]
        i = bisect.bisect_left(self.mod_paths, path)
        del self.mod_paths[i]

    def render_init(self, pkg: str) -> bytes:
        names = self.pkg_all.get(pkg, [])
        body = f'"""The {pkg.rsplit("/", 1)[-1]} package."""\n\n__all__ = [\n'
        body += "".join(f'    "{n}",\n' for n in names)
        return (body + "]\n").encode()

    def render_registry(self) -> bytes:
        body = '"""Handler registry: name to dotted path."""\n\nREGISTRY = {\n'
        body += "".join(f"    {e},\n" for e in self.registry)
        return (body + "}\n").encode()

    def render_changelog(self) -> bytes:
        head = f"# Changelog\n\n## Unreleased (after {self.release})\n\n"
        return (head + "".join(f"- {e}\n" for e in reversed(self.changelog))).encode()

    def new_package(self, ops: list) -> str:
        parent = SRC
        if self.packages and self.rng.random() < 0.3:
            parent = self.rng.choice(self.packages)
        pkg = f"{parent}/{self.word()}_{self.uniq()}"
        bisect.insort(self.packages, pkg)
        self.pkg_all[pkg] = []
        self.put(ops, f"{pkg}/__init__.py", self.render_init(pkg))
        return pkg

    def new_module(self, ops: list) -> str:
        if not self.packages or self.rng.random() < 0.05:
            pkg = self.new_package(ops)
        else:
            pkg = self.rng.choice(self.packages)
        path = f"{pkg}/{self.word()}_{self.uniq()}.py"
        self.add_mod_path(path, self.module())
        self.put_mod(ops, path)
        return path

    def pkg_of(self, path: str) -> str:
        return path.rsplit("/", 1)[0]

    # -- initial project ---------------------------------------------------

    def initial(self) -> Commit:
        ops: list = []
        self.put(
            ops,
            "pyproject.toml",
            (
                "[project]\n"
                f'name = "{PROJECT}"\nversion = "0.1.0"\nrequires-python = ">=3.10"\n\n'
                "[build-system]\n"
                'requires = ["setuptools>=68"]\nbuild-backend = "setuptools.build_meta"\n'
            ).encode(),
        )
        self.put(ops, "README.md", f"# {PROJECT}\n\nA synthetic project for graft benchmarks.\n".encode())
        self.put(ops, f"{SRC}/__init__.py", f'"""{PROJECT}."""\n\n__version__ = "0.1.0"\n'.encode())
        for _ in range(max(4, self.target_modules // 25)):
            self.new_package(ops)
        for _ in range(self.target_modules):
            self.new_module(ops)
        for path in self.mod_paths[:: max(1, len(self.mod_paths) // 50)]:
            self.registry_add(path)
        self.put(ops, REGISTRY, self.render_registry())
        for pkg in self.packages:
            self.put(ops, f"{pkg}/__init__.py", self.render_init(pkg))
        self.changelog = ["Initial import."]
        self.put(ops, CHANGELOG, self.render_changelog())
        return self.commit("init", "initial import of the synthetic project", ops)

    def registry_add(self, path: str) -> None:
        fn = self.rng.choice(self.mods[path].funcs).name
        dotted = path[len("src/") : -3].replace("/", ".")
        bisect.insort(self.registry, f'"{fn}": "{dotted}:{fn}"')

    # -- commit kinds ------------------------------------------------------

    def k_edit_fn(self, ops: list, path: str | None = None) -> str:
        path = path or self.rng.choice(self.mod_paths)
        mod = self.mods[path]
        f = self.rng.choice(mod.funcs)
        names = list(f.args) + [s.split(" = ")[0] for s in f.body]
        for _ in range(self.rng.randrange(1, 4)):
            i = self.rng.randrange(len(f.body))
            new = f.body[i].split(" = ")[0]
            f.body[i] = self.statement(names[: len(f.args) + i] or names, new)
        self.put_mod(ops, path)
        return f"tweak {f.name} in {path.rsplit('/', 1)[-1]}"

    def k_add_fn(self, ops: list) -> str:
        path = self.rng.choice(self.mod_paths)
        f = self.function()
        mod = self.mods[path]
        mod.funcs.insert(self.rng.randrange(len(mod.funcs) + 1), f)
        self.put_mod(ops, path)
        return f"add {f.name}"

    def k_edit_tail(self, ops: list) -> str:
        path = self.rng.choice(self.mod_paths)
        mod = self.mods[path]
        mod.tail.append(f"{self.word().upper()}_{self.uniq()} = {self.rng.randrange(1_000_000)}")
        if len(mod.tail) > 12:
            del mod.tail[0]
        self.put_mod(ops, path)
        return f"tune constants in {path.rsplit('/', 1)[-1]}"

    def k_multi(self, ops: list) -> str:
        count = min(len(self.mod_paths), self.rng.randrange(2, 7))
        for path in self.rng.sample(self.mod_paths, count):
            self.k_edit_fn(ops, path)
        return f"refactor across {count} modules"

    def k_shared(self, ops: list) -> str:
        r = self.rng.random()
        if r < 0.4:
            self.registry_add(self.rng.choice(self.mod_paths))
            self.put(ops, REGISTRY, self.render_registry())
            return "register a handler"
        if r < 0.75:
            path = self.rng.choice(self.mod_paths)
            pkg = self.pkg_of(path)
            name = self.rng.choice(self.mods[path].funcs).name
            if name not in self.pkg_all[pkg]:
                bisect.insort(self.pkg_all[pkg], name)
            self.put(ops, f"{pkg}/__init__.py", self.render_init(pkg))
            return f"export {name}"
        self.changelog.append(f"Change {self.index}: {self.word()} {self.rng.choice(VERBS)} fix.")
        if len(self.changelog) >= CHANGELOG_RELEASE_AT:
            self.release += 1
            notes = f"# Release {self.release}\n\n" + "".join(f"- {e}\n" for e in self.changelog)
            self.put(ops, f"docs/releases/{self.release:05d}.md", notes.encode())
            self.changelog = []
        self.put(ops, CHANGELOG, self.render_changelog())
        return "update the changelog"

    def k_add_module(self, ops: list) -> str:
        if len(self.mod_paths) >= self.max_modules:
            return self.k_delete_module(ops)
        path = self.new_module(ops)
        return f"add module {path.rsplit('/', 1)[-1]}"

    def k_delete_module(self, ops: list) -> str:
        if len(self.mod_paths) <= self.target_modules // 2:
            return self.k_add_module(ops)
        path = self.rng.choice(self.mod_paths)
        self.drop_mod_path(path)
        del self.files[path]
        ops.append(("d", path))
        return f"remove module {path.rsplit('/', 1)[-1]}"

    def k_rename_module(self, ops: list) -> str:
        src = self.rng.choice(self.mod_paths)
        pkg = self.rng.choice(self.packages) if self.rng.random() < 0.3 else self.pkg_of(src)
        dst = f"{pkg}/{self.word()}_{self.uniq()}.py"
        mod = self.mods[src]
        self.drop_mod_path(src)
        self.add_mod_path(dst, mod)
        self.files[dst] = self.files.pop(src)
        ops.append(("r", src, dst))
        if self.rng.random() < 0.5:
            mod.doc = f"The {self.word()} {self.word()} module (moved)."
            self.put_mod(ops, dst)
        return f"rename {src.rsplit('/', 1)[-1]} to {dst.rsplit('/', 1)[-1]}"

    def table_row(self, i: int) -> str:
        return f"{i},{self.word()},{self.rng.randrange(10**9)},{self.rng.randrange(10**6)}"

    def k_large(self, ops: list) -> str:
        if len(self.large) < MAX_LARGE:
            path = f"data/table_{self.uniq()}.csv"
            rows = [self.table_row(i) for i in range(self.rng.randrange(8_000, 40_000))]
            self.large[path] = rows
            what = "add"
        else:
            path = self.rng.choice(sorted(self.large))
            rows = self.large[path]
            for _ in range(max(1, len(rows) // 100)):
                i = self.rng.randrange(len(rows))
                rows[i] = self.table_row(i)
            what = "refresh"
        self.put(ops, path, ("id,name,a,b\n" + "\n".join(rows) + "\n").encode())
        return f"{what} {path}"

    def k_small_many(self, ops: list) -> str:
        d = f"fixtures/{self.word()}_{self.uniq()}"
        self.small_dirs.append(d)
        for i in range(self.rng.randrange(10, 60)):
            self.put(ops, f"{d}/case_{i:03d}.txt", f"{self.word()} {self.rng.randrange(10**6)}\n".encode())
        return f"add fixtures {d.rsplit('/', 1)[-1]}"

    def k_drop_small(self, ops: list) -> str:
        if not self.small_dirs:
            return self.k_small_many(ops)
        d = self.small_dirs.pop(self.rng.randrange(len(self.small_dirs)))
        for path in sorted(p for p in self.files if p.startswith(d + "/")):
            del self.files[path]
            ops.append(("d", path))
        return f"drop fixtures {d.rsplit('/', 1)[-1]}"

    # -- driver ------------------------------------------------------------

    def commit(self, kind: str, message: str, ops: list) -> Commit:
        c = Commit(
            index=self.index,
            kind=kind,
            message=f"{kind}: {message}",
            author=AUTHORS[self.index % len(AUTHORS)],
            timestamp=EPOCH + 60 * self.index,
            ops=ops,
        )
        self.index += 1
        return c

    def next_commit(self) -> Commit:
        kinds, weights = zip(*KINDS)
        kind = self.rng.choices(kinds, weights)[0]
        ops: list = []
        self.changed = False
        message = getattr(self, f"k_{kind}")(ops)
        if not self.changed and not any(op[0] != "w" for op in ops):
            # An edit that happened to reproduce the old text: every commit
            # must change the tree, or `git commit` and `graft commit` refuse.
            message += "; " + self.k_edit_tail(ops)
        return self.commit(kind, message, ops)


def stream(seed: int, n: int, modules: int | None = None):
    """Commit 0 (the initial project), then `n` commits."""
    g = Generator(seed, n, modules)
    yield g.initial()
    for _ in range(n):
        yield g.next_commit()


def digest_commit(h, c: Commit) -> None:
    """Feeds one commit into a running hash of the stream."""
    h.update(f"{c.index}\0{c.message}\0{c.author}\0{c.timestamp}\0".encode())
    for op in c.ops:
        if op[0] == "w":
            h.update(b"w\0" + op[1].encode() + b"\0" + hashlib.sha256(op[2]).digest())
        else:
            h.update("\0".join(op).encode() + b"\0")


def stream_digest(seed: int, n: int) -> str:
    h = hashlib.sha256()
    for c in stream(seed, n):
        digest_commit(h, c)
    return h.hexdigest()
