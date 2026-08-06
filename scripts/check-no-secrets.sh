#!/usr/bin/env sh
# Committed-secrets guard -- a real gap found live 2026-08-06 investigating §5's own
# quality-bar table (goal doc, "Anerkannte Regeln der Technik" row): this row claimed
# partial coverage via "check-no-secrets.sh, the hermetic gate" -- but no such script,
# pre-commit hook, or CI job has ever actually existed in THIS repo. That claim was
# simply stale (a different project's convention referenced but never actually built
# here), and this is a real public GitHub repo with real credential-shaped env vars
# (DEVSYSTEM_GITHUB_TOKEN, CT_CHANNEL_NOISE_KEY/HOLDER_KEY, RAG_EMBEDDING_API_KEY, ...)
# with genuinely nothing scanning for one accidentally landing in a commit.
#
# Exit 0 = clean, non-zero = a likely secret is committed. Detects only genuine
# credential shapes (PEM private keys, cloud access-key ids, GitHub tokens, this
# project's own *_KEY/*_GRANT env-var assignments), so ordinary prose or test
# fixtures containing the word "secret"/"key" do not trip it.
set -eu

# Credential shapes to detect in tracked text files:
#   - PEM private key headers
#   - AWS-style access key ids (AKIA + 16 upper/digits)
#   - Google API keys (AIza + 35 chars)
#   - GitHub tokens: classic/OAuth/user/server/refresh (gh[o/p/r/s/u]_ + 36) and
#     fine-grained PATs (github_pat_ + long) -- this project's own real credential
#     shape for DEVSYSTEM_GITHUB_TOKEN.
pattern='-----BEGIN [A-Z ]*PRIVATE KEY-----|AKIA[0-9A-Z]{16}|AIza[0-9A-Za-z_-]{35}|gh[oprsu]_[0-9A-Za-z]{36}|github_pat_[0-9A-Za-z_]{30,}'
# This project's OWN real credential shape -- a raw 64-hex (32-byte) ed25519/X25519
# key or grant assigned to one of its own real *_KEY/*_GRANT env vars (CT_CHANNEL_*,
# DOCUMENT_EXTRACTION_CHANNEL_*, RAG_EMBEDDING_API_KEY, RAG_UNSTRUCTURED_API_KEY,
# DEVSYSTEM_GITHUB_TOKEN). Anchored to the var name (with `=`/`:`, optional quotes) so
# it catches a real committed key assignment WITHOUT false-positiving on this
# codebase's many bare 64-hex values elsewhere (SHA-256 hashes, real public keys in
# test fixtures, commit refs) -- those are separate variables, not `VAR=<hex>`
# literals matching a known secret name.
pattern="$pattern|(CT_CHANNEL|DOCUMENT_EXTRACTION_CHANNEL)_(HOLDER_KEY|NOISE_KEY|GRANT)[\"']?[[:space:]]*[=:][[:space:]]*[\"']?[0-9a-fA-F]{64}"
pattern="$pattern|(RAG_EMBEDDING_API_KEY|RAG_UNSTRUCTURED_API_KEY|DEVSYSTEM_GITHUB_TOKEN)[\"']?[[:space:]]*[=:][[:space:]]*[\"']?[0-9A-Za-z_.-]{16,}"

# Self-test (regression guard): prove the pattern catches each credential shape and
# does not false-positive on benign text. Synthetic placeholders only -- this file is
# excluded from the scan below, so they never trip the guard itself.
#   Run: scripts/check-no-secrets.sh --selftest
if [ "${1:-}" = "--selftest" ]; then
  rc=0
  for s in \
    'ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA' \
    'gho_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA' \
    'github_pat_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA' \
    'AKIAAAAAAAAAAAAAAAAA' \
    'AIzaAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA' \
    '-----BEGIN RSA PRIVATE KEY-----' \
    'CT_CHANNEL_NOISE_KEY=9f8e7d6c5b4a39281706f5e4d3c2b1a09f8e7d6c5b4a39281706f5e4d3c2b1a0' \
    'export DOCUMENT_EXTRACTION_CHANNEL_GRANT="9f8e7d6c5b4a39281706f5e4d3c2b1a09f8e7d6c5b4a39281706f5e4d3c2b1a0"' \
    'DEVSYSTEM_GITHUB_TOKEN=ghp_realtokenlookingvalue1234567890abcd'
  do
    printf '%s\n' "$s" | grep -Eq -e "$pattern" || { echo "SELFTEST FAIL: pattern missed a credential shape: $s"; rc=1; }
  done
  printf 'a benign line that mentions a secret token value in prose\n' | grep -Eq -e "$pattern" \
    && { echo "SELFTEST FAIL: false positive on benign text"; rc=1; }
  # A bare 64-hex value not anchored to a real secret var name (a SHA-256 hash, a real
  # public key in a test fixture) must NOT trip -- else every push would false-positive
  # on this codebase's many hashes/public-key fixtures.
  printf 'operator_pubkey_hex = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90"\n' \
    | grep -Eq -e "$pattern" && { echo "SELFTEST FAIL: false positive on a bare 64-hex public key/hash"; rc=1; }
  [ "$rc" -eq 0 ] && echo "SELFTEST OK: PEM/AWS/Google/GitHub + this project's own channel-key/grant/token shapes detected, no false positive"
  exit "$rc"
fi

cd "$(cd "$(dirname "$0")/.." && pwd)"
status=0

fail() { echo "SECRET-GUARD FAIL: $1"; status=1; }

# All git-tracked files -- captured ONCE, with the failure explicitly checked. A guard
# that can't run must fail loudly, never silently report clean having scanned nothing.
if ! tracked_files=$(git ls-files); then
  echo "SECRET-GUARD FAIL: git ls-files failed -- cannot verify no secrets are committed"
  exit 1
fi

# Scan tracked text files for the credential shapes in $pattern. runs/ holds real
# per-run state (a run's own persisted signing-key HEX FIELDS are public keys, not
# secrets -- the actual private key lives in a *.key file, already gitignored) but is
# excluded anyway: it's the one directory whose real content is role-filler-controlled
# free text this scanner has no business treating as source.
hits=$(printf '%s\n' "$tracked_files" | while IFS= read -r f; do
  case "$f" in
    *.example|scripts/check-no-secrets.sh|runs/*) continue ;;
  esac
  if grep -EIlq -e "$pattern" "$f" 2>/dev/null; then echo "$f"; fi
done)
if [ -n "$hits" ]; then
  fail "credential material in tracked file(s):"
  echo "$hits"
fi

[ "$status" -eq 0 ] && echo "SECRET-GUARD OK: no committed secrets"
exit "$status"
