import json, glob, statistics, os, sys
root = os.path.expanduser("~/scratch/wi2341r/out")
rows = {}
for f in sorted(glob.glob(root + "/*/results.json")):
    name = f.split("/")[-2]; size, build, i = name.split("-")
    ops = json.load(open(f))["scales"][0]["ops"]
    for k, v in ops.items():
        if isinstance(v, dict) and "graft" in v:
            rows.setdefault((size, k), {}).setdefault(build, []).append((v["graft"]["p50_ms"], v["git"]["p50_ms"]))
for (size, k), b in sorted(rows.items()):
    out = [f"{size:4} {k:14}"]
    for build in ("main", "base"):
        if build in b:
            g = [x[0] for x in b[build]]; t = [x[1] for x in b[build]]
            out.append(f"{build}: graft {statistics.median(g):7.1f} git {statistics.median(t):7.1f} runs {'/'.join(f'{x:.1f}' for x in g)}")
    print(" | ".join(out))
