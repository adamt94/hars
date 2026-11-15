# 0009: The MVP is a reference install; cutover after a shadow week

Status: accepted (2026-11-13)

## Context

"Replace Home Assistant" has no end. A concrete, real install gives a fixed
list of integrations, entities, dashboards and automations to finish, and
real recorded data to test against.

## Decision

- The MVP is **one real Home Assistant 2026.6.1 install (the reference
  install) running on hars with no visible loss of function**, within the
  resource budgets. Its integrations and features are the roadmap's
  inventory; anything else is post-MVP.
- HA's recorder database from that install (10 days) is the test oracle for
  automations; the companion app's sessions must survive the import; app
  encryption is required.
- Notifications, remote access and importing HA's history are deferred.
- Cutover happens only after a week in shadow mode with no differences.
- This was reviewed twice before it was adopted (frontend/API/app, and
  integrations/automation/performance), and those reviews were folded in.

## Consequences

- Scope is small and testable; each roadmap item is checked against the
  reference install.
- Features common elsewhere but absent from the reference install (MQTT,
  ESPHome, templates) wait until after the MVP.
- Personal data from the reference install (its motion timeline) stays out
  of the repository; tests that need it read it from a path given at run
  time.
