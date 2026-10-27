# WI-2611 attester

- Post-land: a restart no longer passes a check over a failure it had
  already posted. A crash while posting a bad verdict's failures left the
  land `pending`, and a flaky check that then passed turned it `good`. The
  restart now finishes the verdict from the failures already posted,
  without running the check, and the land is reverted once.
- Docs: `--retries` must match the line's `retries` setting, since the
  checker does not read it from `GetLineConfig` yet; the `ceil(log2 n) + 1` bound
  counts runs of the failing check only when `retries` is 0.
