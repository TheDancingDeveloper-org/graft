# Frequently asked questions

## Why is graft not compatible with git on the wire?

Graft is server-centric: the server owns workspaces, drafts, rebases, history,
and landing. It therefore does not speak the git wire protocol or store git
objects; familiar command names are used only where their meanings remain
close.

## Can I use graft offline?

A working copy of a remote repository has a bounded cache, so reads of cached
objects continue to work while disconnected. Remote writes wait for the server;
for fully local, single-user work, `graft init` runs the engine in the client
process and needs no network connection.

## Are lines the same as branches?

Lines are a small set of shared, protected histories rather than a feature
branch for every change. Work in progress lives in a leased workspace as a
stack of drafts, and landing rebases that stack on the server before moving the
line with compare-and-swap.

## How do changes move between lines?

`graft line merge` brings one line into another with a three-way merge and
records a merge point between them. The source line is read, not moved, and an
unresolved merge is returned to the working copy for resolution.

## How do conflicts reach me?

Graft reports conflicting paths and leaves the shared line unchanged. After
`graft sync` or `graft land --sync`, text conflicts appear as markers in the
files and other conflicts use `.graft-conflict` sidecars, so they can be
resolved and committed from the working copy.

## Where does CI run?

In phase one, graft runs no build, test, check, or checker anywhere, including
the land path. For git-primary repositories, CI stays on the forge and runs on
the batch pull request that carries landed changes back to git.

## Can I go back to git?

Yes. `graft mirror export` writes the full history back to git, and import keeps
raw tag objects so migration does not discard them; deterministic full export
is the exit path, not an afterthought.

## Is graft faster than git?

It depends on the operation and scale. Recorded results show graft ahead on
full and sparse clone, path log, blame, and some concurrent lands, but behind
on recent log, status, and sync; see the [benchmark results](benchmarks.md) for
the measurements and limitations.

## What does AGPL-3.0-only mean for an internal server?

The licence is AGPL-3.0-only; if users interact over a network with your
modified version, those users must be offered its corresponding source. See
the [FSF licence FAQ](https://www.gnu.org/licenses/gpl-faq.html#AGPLv3InteractingRemotely)
for the rule and consult your own counsel for legal advice.

## Which platforms are supported?

The documented release archives target Linux on x86_64 and aarch64, plus macOS
on Apple Silicon. There is no Intel macOS archive, and no Windows archive is
currently documented.
