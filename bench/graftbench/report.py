"""Machine info and the Markdown summary."""

from __future__ import annotations

import os
import platform
from pathlib import Path

from .proc import run


def machine(git_version: str, graft_version: str, repo_root: Path) -> dict:
    cpu = ""
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                cpu = line.split(":", 1)[1].strip()
                break
    except OSError:
        pass
    mem_kib = None
    try:
        for line in Path("/proc/meminfo").read_text().splitlines():
            if line.startswith("MemTotal:"):
                mem_kib = int(line.split()[1])
    except OSError:
        pass
    sha = run(["git", "rev-parse", "HEAD"], cwd=repo_root, check=False).out.strip()
    dirty = bool(run(["git", "status", "--porcelain", "--", "crates", "Cargo.lock"], cwd=repo_root, check=False).out.strip())
    return {
        "host": platform.node(),
        "os": f"{platform.system()} {platform.release()}",
        "cpu": cpu,
        "cpus": os.cpu_count(),
        "mem_gib": round(mem_kib / 1048576, 1) if mem_kib else None,
        "python": platform.python_version(),
        "git": git_version,
        "graft_version": graft_version,
        "graft_commit": sha,
        "graft_tree_dirty": dirty,
    }


def _ms(d: dict, key: str = "p50_ms") -> str:
    v = d.get(key)
    return "" if v is None else f"{v:,.1f}"


def _yes(v) -> str:
    return "yes" if v else "**NO**"


def markdown(doc: dict) -> str:
    m = doc["machine"]
    out = [
        f"# graft bench: {doc['label']}",
        "",
        f"Started {doc['started']}, {doc['elapsed_s']:.0f} s. Seed {doc['seed']}, "
        f"{doc['writers']} concurrent writers x {doc['rounds']} rounds, {doc['iterations']} iterations per operation. "
        f"Server: {doc['server']}. All lines `direct` (phase 1, D22): no checks in the land path.",
        "",
        "| Machine | |",
        "| --- | --- |",
        f"| Host | {m['host']}, {m['os']} |",
        f"| CPU | {m['cpu']} ({m['cpus']} CPUs), {m['mem_gib']} GiB |",
        f"| graft | `{m['graft_commit'][:12]}`{' (dirty)' if m['graft_tree_dirty'] else ''}, {m['graft_version']} |",
        f"| git | {m['git']} |",
        f"| Python | {m['python']} |",
        "",
    ]
    for r in doc["scales"]:
        out += scale_md(r)
    return "\n".join(out) + "\n"


def scale_md(r: dict) -> list[str]:
    if r.get("kind") == "many_copies":
        return copies_md(r)
    g = r["generate"]
    out = [
        f"## Scale {r['scale']:,}",
        "",
        f"{g['commits']:,} commits ({g['ops']:,} file operations), final tree {g['final_files']:,} files, "
        f"{g['final_bytes'] / 1048576:.1f} MiB. Stream sha256 `{g['stream_sha256'][:16]}`, "
        f"git head `{g['ref_head'][:12]}`. Result: **{'pass' if r['ok'] else 'FAIL'}**.",
        "",
        "### History",
        "",
        "| Step | graft | git |",
        "| --- | ---: | ---: |",
    ]
    gr, gt = r["graft_replay"], r["git_replay"]
    out += [
        f"| replay throughput (commits/s, commit + land or push) | {gr['commits_per_s']} | {gt['commits_per_s']} |",
        f"| commit p50 / p95 (ms) | {_ms(gr['commit'])} / {_ms(gr['commit'], 'p95_ms')} | {_ms(gt['commit'])} / {_ms(gt['commit'], 'p95_ms')} |",
        f"| land or push p50 / p95 / p99 (ms) | {_ms(gr['land'])} / {_ms(gr['land'], 'p95_ms')} / {_ms(gr['land'], 'p99_ms')} "
        f"| {_ms(gt['push'])} / {_ms(gt['push'], 'p95_ms')} / {_ms(gt['push'], 'p99_ms')} |",
    ]
    if "import" in r:
        i = r["import"]
        out.append(
            f"| import whole history (s) | {i['graft_mirror_import_s']} (graft-mirror, {i['commits_per_s']} commits/s) "
            f"| {i['git_fast_import_s']} (fast-import + generation) |"
        )
    out += ["", "### Operations (p50 ms)", "", "| Operation | graft | git | graft ÷ git |", "| --- | ---: | ---: | ---: |"]
    ops = r["ops"]
    labels = {
        "clone_full": "clone, full",
        "clone_sparse": f"clone, sparse (`{ops['sparse_prefix']}`)",
        "status": f"status ({ops['status_edits']} changed files)",
        "diff_worktree": "diff, working copy",
        "diff_range": f"diff, {ops['diff_range_changesets']} changesets",
        "log": "log, 50",
        "log_path": f"log --path `{ops['hot_path'].rsplit('/', 1)[-1]}`, 50",
        "blame": f"blame `{ops['hot_path'].rsplit('/', 1)[-1]}`",
        "sync": f"sync, {ops['sync_behind']} behind",
    }
    for key, label in labels.items():
        if key in ops:
            o = ops[key]
            out.append(f"| {label} | {_ms(o.get('graft', {}))} | {_ms(o.get('git', {}))} | {o.get('graft_over_git_p50') or ''} |")
    c = r["concurrent"]
    out += [
        "",
        f"### Concurrent writers ({c['writers']} writers x {c['rounds']} rounds)",
        "",
        "| Scenario | System | landed | p50 | p95 | p99 (ms) | lands/s | contended | attempts | land refused (file level) | conflicts surfaced | first conflict p50 (ms) |",
        "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    for s in ("clean", "merge3", "conflict"):
        for system in ("graft", "git"):
            x = c[system][s]
            lat = x["latency"]
            out.append(
                f"| {s} | {system} | {x['landed']}/{x['lands']} | {_ms(lat)} | {_ms(lat, 'p95_ms')} | {_ms(lat, 'p99_ms')} "
                f"| {x['throughput_lands_per_s']} | {x['contended']} | {x['attempts']} | {x['file_level_refusals'] if system == 'graft' else ''} "
                f"| {x['conflicts_surfaced']} "
                f"| {_ms(x['conflict_surface'])} |"
            )
    v, f = r["verify_replay"], r["verify_final"]
    out += [
        "",
        "### Correctness",
        "",
        "| Check | Result |",
        "| --- | --- |",
        f"| git replay head = fast-import head (byte-identical history) | {_yes(v['git_replay_head_equals_fast_import_head'])} |",
        f"| graft tree after replay = model replay ({v['graft_tree_vs_model'].get('files', 0):,} files) | {_yes(v['graft_tree_vs_model']['equal'])} |",
        f"| git tree after replay = model replay | {_yes(v['git_tree_vs_model']['equal'])} |",
        f"| graft log = landed sequence ({v['graft_log_length']:,} changesets) | {_yes(v['graft_log_equals_landed_sequence'])} |",
    ]
    if "import" in r:
        out.append(f"| graft-mirror import tree = model | {_yes(r['import']['final_tree_matches_model'])} |")
    out += [
        f"| final graft tree = expected, byte for byte | {_yes(f['graft_vs_expected']['equal'])} |",
        f"| final git tree = expected, byte for byte | {_yes(f['git_vs_expected']['equal'])} |",
        f"| final graft tree = final git tree | {_yes(f['graft_vs_git']['equal'])} |",
        f"| no lost or duplicated lands ({f['lands_recorded']:,} lands, log {f['graft_log_length']:,}) | {_yes(f['no_lost_or_duplicated_lands'])} |",
        *[
            f"| blame of `{b['path'].rsplit('/', 1)[-1]}` intact ({b['lines']:,} lines; agrees with git on {b['agree_with_git']:,}) | {_yes(b['intact'])} |"
            for b in f.get("blame", [])
        ],
        f"| line never moved backwards (replay order, {f['tips_observed']} tips polled during concurrent lands) | {_yes(f['line_never_moved_backwards'])} |",
    ]
    sv = r["server"]
    if sv.get("end"):
        out += ["", "### Server", "", "| Point | CPU s | RSS MiB | peak RSS MiB | storage MiB |", "| --- | ---: | ---: | ---: | ---: |"]
        for point, x in sv.items():
            out.append(
                f"| {point} | {x.get('cpu_s', '')} | {_mib(x.get('rss_kib'), 1024)} | {_mib(x.get('peak_rss_kib'), 1024)} "
                f"| {_mib(x.get('storage_bytes'), 1048576)} |"
            )
        out.append(f"\ngit replay repository: {gt['repo_bytes'] / 1048576:.1f} MiB (loose objects, no gc).")
    out.append("")
    return out


def _mib(v, div) -> str:
    return "" if v is None else f"{v / div:.1f}"


def copies_md(r: dict) -> list[str]:
    """The many-working-copies scenario (WI-2313)."""
    rp = r["repository"]
    out = [
        f"## Many working copies, scale {r['scale']:,}: {'pass' if r['ok'] else '**FAIL**'}",
        "",
        f"Repository: {rp['files']:,} files, {rp['bytes'] / 1048576:.1f} MiB; sparse slice `{rp['sparse_prefix']}`: "
        f"{rp['sparse_files']:,} files, {rp['sparse_bytes'] / 1048576:.2f} MiB. The N graft clones start at once; "
        "git is one clone plus N-1 `git worktree add`, one at a time, sharing one object store.",
        "",
        "| System | Shape | N | Copies ok | Wall s | Clone p50 ms | Clone p95 ms | Files MiB | Cache MiB | Other state MiB | Total MiB | Per copy MiB |",
        "| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    for x in r["runs"]:
        b = x["bytes"]
        out.append(
            f"| {x['system']} | {x['shape']} | {x['n']} | {_yes(x['copies_ok'])} | {x['wall_s']:.2f} | "
            f"{_ms(x['clone'])} | {_ms(x['clone'], 'p95_ms')} | {_mib(b['files'], 1048576)} | {_mib(b['cache'], 1048576)} | "
            f"{_mib(b['meta'], 1048576)} | {_mib(b['total'], 1048576)} | {_mib(x['bytes_per_copy']['total'], 1048576)} |"
        )
    out += [
        "",
        "graft's cache is per clone (`.graft/cache`), so it grows with N; git's object store is shared, counted once "
        "under other state. Server storage growth over the run: "
        f"{r['server_storage_growth_bytes']:,} bytes. graft-node: {r['node']}.",
        "",
    ]
    return out
