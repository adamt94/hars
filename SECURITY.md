# Security policy

## Supported versions

hars has no releases yet. Only the latest commit on `main`, and the
`ghcr.io/adamt94/homeassistant-rust:latest` image built from it, get fixes.

## Reporting a vulnerability

Please report it privately, not in a public issue: use GitHub's private
vulnerability reporting (the repository's **Security** tab, then **Report a
vulnerability**). Include what an attacker can do, the steps to reproduce,
and the commit or image you tested.

You can expect an answer within a week. Once a fix is merged, the advisory is
published with credit to you, unless you prefer not to be named.

If the issue is in Home Assistant itself (its frontend or companion apps,
which hars serves unchanged), report it to the Home Assistant project as
well.

## What hars protects, and what it doesn't

- `config/.storage` holds password hashes (bcrypt), refresh tokens and
  integrations' keys and tokens. hars writes those files readable by their
  owner only, as Home Assistant does. Backups contain them too.
- Every `/api/*` call needs a token, as in Home Assistant; the companion
  app's webhook is authenticated by its secret id and encrypted.
- hars is built for a home network. It has not been hardened for exposure to
  the internet, and remote access is not supported yet. Don't forward port
  8123 to it; use a VPN.
