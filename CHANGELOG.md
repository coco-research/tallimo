# Changelog

All notable changes to this project are documented in this file. The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

- Finished. No longer developed. Released under the MIT licence. Later versions were not recorded in this file.

## [0.2.1] - 2026-10-10

### Changed

- The sample's merged pull requests carry merge dates relative to today, so the Monday note always shows recent merges and Leadership counts recent work (#43).

### Fixed

- Live sync gives up on a Jira or GitHub call after 20 seconds instead of holding every other request. Fixture mode is unaffected (#45).
- Leadership shows an error when a per-person count fails, instead of showing nobody committed (#46).

## [0.2.0] - 2026-10-07

### Upgrade

- Coming from a v0.1.0 database: stop the server, move `crates/rollup.db*` aside to a dated backup outside the repo (for example `~/tallimo-backups/rollup-YYYY-MM-DD/`), then start v0.2.0 and load the sample once with `curl -X POST http://127.0.0.1:8787/sync`. Without this, Sync fails with a 500 (`UNIQUE constraint failed: identity_map.slack_member_id`) and the old people stay in the database. Never delete the old database: it may hold real data.

### Known issues

- Not yet clicked through by a person on the release build.
- Role checks are client-asserted (there is no session identity), so run it on 127.0.0.1 only.

### Added

- The first screen asks who you are. Each role then opens on its own page.
- A developer attaches GitHub repos. A PM attaches Jira boards. Attaching the wrong kind is refused.
- Each merged pull request now keeps its merge date, and the workspace data returns it.
- How to report a security problem (SECURITY.md), plus issue and pull request templates.

### Changed

- Every page uses the Signal Consult look, in light and dark, and at phone width. The pages are Today, Inbox, Your desk, Your update, People, Projects, Activity, Leadership, Monday note, Connect, and Settings.
- The people in the sample fixture are fictional, and the README screenshots show them.
- Leadership counts a person as committed to an epic from their story assignments and their pull requests from the last 10 business days, not from every pull request they ever opened.
- Dependencies updated: reqwest 0.13, tower-http 0.7, actions/checkout 7.

### Fixed

- Week windows count a merge on the local calendar day it happened, not its UTC date.
- Toast messages show on the onboard screen, for example "Cleared."
- The principal PM no longer sees Sync.
- The build finishes with zero warnings.
- A write (`/sync`, `/match`, `/map`, `/attach`, the review queue and project selection) whose Origin header names another site is refused with 403, so a page on another site can no longer start a sync or a match. Requests with no Origin, such as curl, still work (#33).
- Attaching on Connect (`/map` and `/projects/select`) refuses a missing or roleless person instead of writing for them (#29).
- A refused confirm in the review queue keeps the link the pull request already has; the confirm runs in one transaction (#30).
- A request whose Host is not this server (127.0.0.1, localhost or [::1], with or without its port) is refused with 421 before anything is read or written, so a page on a DNS-rebinding domain cannot reach the API (#32).

## [0.1.0] - 2026-10-03

### Added

- Run it locally in fixture mode. From `crates`, `cargo run -p rollup-api` serves the page at http://127.0.0.1:8787 from the sample fixture. Post to `/sync` to reload that sample. The page is one HTML file compiled into the binary.
- Each GitHub pull request is matched to a Jira story by the story key in its title, body, or branch. The rollup shows planned, in flight, and done for each epic.
- Team view is the daily pass: a review queue where you link or ignore a pull request that did not match, then each project's delivered, in flight, planned, and stalled work.
- Leadership view shows outcomes, outcomes by project, capacity commitment, and a three-month forecast.
- CI runs the tests on GitHub Actions. One run is kept per ref. The README has a hero, light and dark screenshots, a tested quick start, and a how-it-works diagram.

