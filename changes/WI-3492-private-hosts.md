- `graft-mirror serve` can poll an upstream on a private address when the
  serve config names its host in `private_hosts_allowed` (exact names, no
  wildcards; empty by default, which keeps today's refusal). Only RFC 1918,
  CGNAT (100.64.0.0/10) and unique-local addresses are excused for a named
  host; loopback, link-local and reserved ranges, and literal addresses in the
  URL, are still refused. Only an `https://` upstream is excused: ssh is not
  pinned to the vetted address. git stays pinned to the vetted address and follows
  no redirect.
