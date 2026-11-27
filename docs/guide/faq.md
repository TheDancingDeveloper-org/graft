# Frequently asked questions

Short answers to common questions about graft, with links to the page that covers each in depth.

## Why is graft not compatible with git on the wire?

Graft is server-centric: the server owns workspaces, drafts, rebases, history and landing. It therefore does not speak the git wire protocol or store git objects. Familiar command names are used only where their meanings remain close.

See also: [Concepts](concepts.md).

## Can I use graft offline?

Partly. A working copy of a remote repository has a bounded cache, so reads of cached objects continue to work while disconnected, but remote writes wait for the server. For fully local, single-user work, `graft init` runs the engine in the client process and needs no network connection.

See also: [Command reference](cli.md).

## Are lines the same as branches?

No. Lines are a small set of shared, protected histories rather than a feature branch for every change. Work in progress lives in a leased workspace as a stack of drafts, and landing rebases that stack on the server before moving the line with compare-and-swap.

See also: [Concepts](concepts.md).

## How do changes move between lines?

`graft line merge` brings one line into another with a three-way merge and records a merge point between them. The source line is read, not moved, and an unresolved merge is returned to the working copy for resolution.

See also: [Concepts](concepts.md#line-merge).

## How do conflicts reach me?

Graft reports the conflicting paths and leaves the shared line unchanged. After `graft sync` or `graft land --sync`, text conflicts appear as markers in the files and other conflicts use `.graft-conflict` sidecars, so you can resolve and commit them from the working copy.

See also: [Concepts](concepts.md#conflict), [Safety](safety.md).

## Where does CI run?

Graft itself runs no build, test, check or checker anywhere in this phase, including on the land path. For git-primary repositories, CI stays on the forge and runs on the batch pull request that carries landed changes back to git.

See also: [CI attestation](ci-attest.md).

## Can I go back to git?

Yes. `graft mirror export` writes the full history back to git, and import keeps raw tag objects so migration does not discard them. Deterministic full export is the exit path, not an afterthought.

See also: [Git migration](git-migration.md), [Stability](stability.md).

## Is graft faster than git?

It depends on the operation and the scale. Recorded results show graft ahead on full and sparse clone, path log, blame and some concurrent lands, and behind on recent log, status and sync. The measurements and their limits are in the benchmark page.

See also: [Benchmarks](benchmarks.md).

## What does AGPL-3.0-only mean for an internal server?

The licence is AGPL-3.0-only. If users interact over a network with your modified version, those users must be offered its corresponding source. See the [FSF licence FAQ](https://www.gnu.org/licenses/gpl-faq.html#AGPLv3InteractingRemotely) for the rule, and consult your own counsel for legal advice.

## Which platforms are supported?

Release archives are published for Linux on x86_64 and aarch64 (musl). macOS and Windows binaries are not published and are not supported.

See also: [Stability](stability.md#platforms).
