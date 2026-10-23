#!/usr/bin/env python3
"""Regenerates the recorded OIDC fixtures used by tests/oidc.rs.

Everything here is throwaway: the RSA and Ed25519 keys are generated on each
run and discarded, and only the public JWKS and the signed tokens are written.
No real issuer, key or secret is involved. The tests pass a fixed `now`
(NOW below), so the recorded tokens never need to be fresh.

    python3 -I crates/graft-auth/tests/fixtures/oidc/gen.py

Needs the `cryptography` package.
"""
import base64
import hashlib
import hmac
import json
import pathlib

from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.asymmetric import ed25519, padding, rsa

OUT = pathlib.Path(__file__).resolve().parent
NOW = 1_800_000_000

GH_ISS = "https://token.actions.githubusercontent.com"
GH_AUD = "https://graft.example.test"
WP_ISS = "https://ci.example.test"
WORKFLOW = ("TheDancingDeveloper-org/graft-ci/.github/workflows/"
            "graft-pre-land.yml@refs/heads/main")


def b64(raw: bytes) -> str:
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


def jint(n: int) -> str:
    return b64(n.to_bytes((n.bit_length() + 7) // 8, "big"))


def jwk_rsa(key, kid, **extra):
    pub = key.public_key().public_numbers()
    return {"kty": "RSA", "kid": kid, "use": "sig", "alg": "RS256",
            "n": jint(pub.n), "e": jint(pub.e), **extra}


def jwt(header, payload, sign):
    h = b64(json.dumps(header, separators=(",", ":")).encode())
    p = b64(json.dumps(payload, separators=(",", ":")).encode())
    return f"{h}.{p}.{b64(sign(f'{h}.{p}'.encode()))}"


def rs256(key):
    return lambda m: key.sign(m, padding.PKCS1v15(), hashes.SHA256())


def gh_claims(**over):
    c = {
        "iss": GH_ISS,
        "aud": GH_AUD,
        "sub": "repo:TheDancingDeveloper-org/graft:ref:refs/heads/graft/wi-2265",
        "repository": "TheDancingDeveloper-org/graft",
        "repository_id": "900000001",
        "repository_owner": "TheDancingDeveloper-org",
        "ref": "refs/heads/graft/wi-2265",
        "job_workflow_ref": WORKFLOW,
        "event_name": "push",
        "iat": NOW - 60,
        "nbf": NOW - 60,
        "exp": NOW + 240,
    }
    c.update(over)
    return {k: v for k, v in c.items() if v is not None}


def main():
    gh = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    other = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    small = rsa.generate_private_key(public_exponent=65537, key_size=1024)
    wp = ed25519.Ed25519PrivateKey.generate()

    hdr = {"alg": "RS256", "typ": "JWT", "kid": "gh-test-1"}
    sign = rs256(gh)
    t = {}
    t["gh_ok"] = jwt(hdr, gh_claims(), sign)
    t["gh_aud_array"] = jwt(hdr, gh_claims(aud=["other.example.test", GH_AUD]), sign)
    t["gh_mixed_case_repo"] = jwt(
        hdr, gh_claims(repository="thedancingdeveloper-org/GRAFT"), sign)
    t["gh_other_repo"] = jwt(
        hdr, gh_claims(repository="TheDancingDeveloper-org/other",
                       sub="repo:TheDancingDeveloper-org/other:ref:refs/heads/graft/x"), sign)
    t["gh_other_owner_same_name"] = jwt(
        hdr, gh_claims(repository="attacker/graft", repository_owner="attacker"), sign)
    t["gh_reused_name_new_id"] = jwt(hdr, gh_claims(repository_id="900000099"), sign)
    t["gh_ref_release"] = jwt(hdr, gh_claims(ref="refs/heads/release/1.0"), sign)
    t["gh_ref_prefix_trick"] = jwt(hdr, gh_claims(ref="refs/heads/main-evil"), sign)
    t["gh_post_land_ref"] = jwt(hdr, gh_claims(ref="refs/heads/main"), sign)
    t["gh_own_workflow"] = jwt(hdr, gh_claims(
        job_workflow_ref="TheDancingDeveloper-org/graft/.github/workflows/evil.yml"
                         "@refs/heads/graft/wi-2265"), sign)
    t["gh_workflow_other_ref"] = jwt(hdr, gh_claims(job_workflow_ref=WORKFLOW[:-4] + "evil"), sign)
    t["gh_no_workflow"] = jwt(hdr, gh_claims(job_workflow_ref=None), sign)
    t["gh_no_ref"] = jwt(hdr, gh_claims(ref=None), sign)
    t["gh_wrong_aud"] = jwt(hdr, gh_claims(aud="https://other.example.test"), sign)
    t["gh_no_aud"] = jwt(hdr, gh_claims(aud=None), sign)
    t["gh_wrong_iss"] = jwt(hdr, gh_claims(iss="https://evil.example.test"), sign)
    t["gh_signed_claiming_wp_issuer"] = jwt(hdr, gh_claims(iss=WP_ISS), sign)
    t["gh_expired"] = jwt(hdr, gh_claims(exp=NOW - 1), sign)
    t["gh_exp_now"] = jwt(hdr, gh_claims(exp=NOW), sign)
    t["gh_no_exp"] = jwt(hdr, gh_claims(exp=None), sign)
    t["gh_not_before"] = jwt(hdr, gh_claims(nbf=NOW + 3600), sign)
    t["gh_future_iat"] = jwt(hdr, gh_claims(iat=NOW + 3600), sign)
    t["gh_long_lived"] = jwt(hdr, gh_claims(exp=NOW + 7200), sign)
    t["gh_short_remaining"] = jwt(hdr, gh_claims(exp=NOW + 30), sign)
    t["gh_no_kid"] = jwt({"alg": "RS256", "typ": "JWT"}, gh_claims(), sign)
    t["gh_unknown_kid"] = jwt({**hdr, "kid": "nope"}, gh_claims(), sign)
    t["gh_wrong_key"] = jwt(hdr, gh_claims(), rs256(other))
    t["gh_small_key"] = jwt({**hdr, "kid": "small-1"}, gh_claims(), rs256(small))
    t["gh_alg_none"] = jwt({"alg": "none", "typ": "JWT", "kid": "gh-test-1"}, gh_claims(),
                           lambda m: b"")
    n = gh.public_key().public_numbers().n
    secret = n.to_bytes((n.bit_length() + 7) // 8, "big")
    t["gh_hs256_public_key"] = jwt(
        {**hdr, "alg": "HS256"}, gh_claims(),
        lambda m: hmac.new(secret, m, hashlib.sha256).digest())
    t["gh_eddsa_on_rsa_key"] = jwt({**hdr, "alg": "EdDSA"}, gh_claims(), lambda m: b"\0" * 64)
    # A valid token whose payload is swapped for another repo's, signature kept.
    h, _, s = t["gh_ok"].split(".")
    forged = b64(json.dumps(gh_claims(repository="TheDancingDeveloper-org/other"),
                            separators=(",", ":")).encode())
    t["gh_tampered_payload"] = f"{h}.{forged}.{s}"
    # The same token with trailing garbage and with a missing signature.
    t["gh_four_parts"] = t["gh_ok"] + ".AAAA"
    t["gh_two_parts"] = ".".join(t["gh_ok"].split(".")[:2])

    wp_claims = {
        "iss": WP_ISS, "aud": GH_AUD, "sub": "indexarr/graft/pipeline/7",
        "repo": "indexarr/graft", "repo_id": 42, "ref": "refs/heads/graft/wi-2265",
        "event": "push", "iat": NOW - 30, "exp": NOW + 270,
    }
    wp_hdr = {"alg": "EdDSA", "typ": "JWT", "kid": "wp-test-1"}
    t["wp_ok"] = jwt(wp_hdr, wp_claims, wp.sign)
    t["wp_other_repo"] = jwt(wp_hdr, {**wp_claims, "repo": "indexarr/other"}, wp.sign)
    t["wp_wrong_key"] = jwt(wp_hdr, wp_claims, ed25519.Ed25519PrivateKey.generate().sign)

    pub = wp.public_key().public_bytes_raw()
    jwks = {
        "keys": [
            jwk_rsa(gh, "gh-test-1"),
            jwk_rsa(other, "gh-test-2"),
            jwk_rsa(small, "small-1"),
            # A key type graft does not verify, and an encryption key: skipped.
            {"kty": "EC", "kid": "ec-1", "use": "sig", "crv": "P-256",
             "x": b64(b"\1" * 32), "y": b64(b"\2" * 32)},
            jwk_rsa(other, "enc-1", use="enc"),
        ]
    }
    wp_jwks = {"keys": [{"kty": "OKP", "crv": "Ed25519", "kid": "wp-test-1",
                          "use": "sig", "alg": "EdDSA", "x": b64(pub)}]}
    (OUT / "github-jwks.json").write_text(json.dumps(jwks, indent=2) + "\n")
    (OUT / "woodpecker-jwks.json").write_text(json.dumps(wp_jwks, indent=2) + "\n")
    (OUT / "tokens.json").write_text(json.dumps(t, indent=2, sort_keys=True) + "\n")


main()
