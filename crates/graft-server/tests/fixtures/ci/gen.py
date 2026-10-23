#!/usr/bin/env python3
"""Regenerates the recorded CI tokens used by tests/ci.rs.

Throwaway keys, generated on each run and discarded: only the public JWKS
and the signed tokens are written. The server under test reads the real
clock, so the tokens are issued in the past (IAT) and expire far in the
future (EXP); the test config raises `max_lifetime_s` to accept them. No
real issuer is involved.

    python3 -I crates/graft-server/tests/fixtures/ci/gen.py

Needs the `cryptography` package.
"""
import base64
import json
import pathlib

from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.asymmetric import padding, rsa

OUT = pathlib.Path(__file__).resolve().parent
IAT = 1_760_000_000  # 2025-10-09
EXP = 4_000_000_000  # 2096
ISS = "https://token.actions.githubusercontent.com"
AUD = "https://graft.e2e.test"
WORKFLOW = "o/graft-ci/.github/workflows/graft-pre-land.yml@refs/heads/main"
# The commit the job ran on (GitHub's `sha` claim). The test maps the
# draft it attests to this commit.
SHA = "1f2e3d4c5b6a79881f2e3d4c5b6a79881f2e3d4c"


def b64(raw: bytes) -> str:
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


def jint(n: int) -> str:
    return b64(n.to_bytes((n.bit_length() + 7) // 8, "big"))


def jwt(key, kid, payload):
    h = b64(json.dumps({"alg": "RS256", "typ": "JWT", "kid": kid},
                       separators=(",", ":")).encode())
    p = b64(json.dumps(payload, separators=(",", ":")).encode())
    sig = key.sign(f"{h}.{p}".encode(), padding.PKCS1v15(), hashes.SHA256())
    return f"{h}.{p}.{b64(sig)}"


def claims(repo, repo_id, **over):
    c = {
        "iss": ISS, "aud": AUD,
        "sub": f"repo:{repo}:ref:refs/heads/graft/e2e",
        "repository": repo, "repository_id": repo_id,
        "ref": "refs/heads/graft/e2e", "sha": SHA, "job_workflow_ref": WORKFLOW,
        "iat": IAT, "nbf": IAT, "exp": EXP,
    }
    c.update(over)
    return c


def main():
    key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    rotated = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    t = {
        # o/app is bound to graft repo `r`; o/other to graft repo `other`;
        # o/reader may only read `r`.
        "ok": jwt(key, "e2e-1", claims("o/app", "101")),
        "other_commit": jwt(key, "e2e-1", claims("o/app", "101", sha="9" * 40)),
        "reader": jwt(key, "e2e-1", claims("o/reader", "104")),
        "other_repo": jwt(key, "e2e-1", claims("o/other", "102")),
        "unbound": jwt(key, "e2e-1", claims("o/stranger", "103")),
        "own_workflow": jwt(key, "e2e-1", claims(
            "o/app", "101",
            job_workflow_ref="o/app/.github/workflows/evil.yml@refs/heads/graft/e2e")),
        "wrong_aud": jwt(key, "e2e-1", claims("o/app", "101", aud="https://other.test")),
        # Signed by a key only the second JWKS holds (a rotation).
        "rotated": jwt(rotated, "e2e-2", claims("o/app", "101")),
    }

    def jwk(k, kid):
        pub = k.public_key().public_numbers()
        return {"kty": "RSA", "kid": kid, "use": "sig", "alg": "RS256",
                "n": jint(pub.n), "e": jint(pub.e)}

    (OUT / "jwks.json").write_text(json.dumps({"keys": [jwk(key, "e2e-1")]}, indent=2) + "\n")
    (OUT / "jwks-rotated.json").write_text(json.dumps(
        {"keys": [jwk(key, "e2e-1"), jwk(rotated, "e2e-2")]}, indent=2) + "\n")
    (OUT / "tokens.json").write_text(json.dumps(t, indent=2, sort_keys=True) + "\n")


main()
