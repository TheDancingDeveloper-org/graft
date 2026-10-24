- graft-engine and graft-gc: a test-only `planted-bugs` cargo feature, off by
  default, with hooks for the adversarial harness's four planted bugs
  (metadata before objects, a skipped land idempotency check, a lease
  boundary one tick early, GC ignoring the land-attempt holder). A
  `PlantedBugs` handle (in graft-gc, re-exported by graft-engine) switches
  each on at runtime; without the feature the hooks compile away (WI-2345).
