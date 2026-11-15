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
- REST state, config, service and mobile registration routes require an
  `Authorization: Bearer` access token. `/auth/*` provides login and token
  exchange. Until onboarding is finished, as in Home Assistant, these are
  anonymous: `/api/onboarding` and `/api/onboarding/installation_type`
  (status), `/api/onboarding/integration/wait`, and
  `/api/onboarding/users`, which creates the owner. Anyone who reaches a
  fresh install before you can make themselves its owner, so onboard it
  right after the first start. Once the owner exists, the user step refuses
  to run again; after a restart with onboarding finished, none of these
  routes exist. `/api/websocket` authenticates inside the WebSocket
  protocol. Brands routes use their own access token.
- hars never sends analytics. Onboarding's analytics choice is stored in
  `.storage/core.analytics` (Home Assistant's file) and nothing else.
  Location detection during onboarding asks Home Assistant's own service
  (`services.home-assistant.io/whoami`) only when you agree to it in the
  page, as Home Assistant does; the met forecast asks `api.met.no` for your
  home's coordinates.
- Companion app webhooks use the secret webhook id as their credential.
  Encryption is required when the registration has `supports_encryption`
  enabled; registrations without that flag can send plaintext. Treat webhook
  ids as passwords, including in URLs and logs.
- hars is built for a home network. It has not been hardened for exposure to
  the internet, and remote access is not supported yet. Don't forward port
  8123 to it; use a VPN.
