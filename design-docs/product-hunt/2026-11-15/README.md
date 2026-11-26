# Product Hunt refresh — 2026-11-15

Product: [CCUsage Gauge](https://www.producthunt.com/products/ccusage-gauge?launch=ccusage-gauge)

Source: repository version 0.4.3, commit e615cee. The current frontend was built
with `mise run build`. The three dashboard captures use the shipped frontend with isolated,
synthetic API responses matching `frontend/src/api.ts`; no account credentials,
real usage, hostnames, or project paths appear. Each dashboard gallery image is labeled
"Example data". The demo uses one local machine and does not demonstrate SSH
collection. Browser chrome was cropped; product controls and values were retained.

## Copy

Name: CCUsage Gauge

Tagline: Track Claude Code & Codex limits, credits and costs

Description:

Know how much Claude Code and Codex allowance you have left. See 5-hour and weekly usage, reset countdowns, Claude extra usage and Codex credits, alongside token counts and API-equivalent cost history. Free and open source, with a macOS menu-bar app, native or browser dashboard, and a CLI for macOS and Linux. Explore usage by model, agent, machine and directory; connect remote machines over SSH. Install with Homebrew.

## Media

- `thumbnail.png`: 240 × 240, resized from the current `Resources/AppIcon.png`.
  Replaces the previous gold logo with the app's flat monochrome usage pie.
- `01-dashboard.png`: 1270 × 760, light dashboard with subscription and cost views.
- `02-subscription-usage.png`: 1270 × 760, readable subscription and credit detail.
- `03-model-directory-filters.png`: 1270 × 760, dark filtered Codex model view.

## Maker comment

Hi Product Hunt,

I built CCUsage Gauge to keep AI coding usage visible without repeatedly checking reports. It brings Claude Code and Codex subscription allowance and cost history into one place.

See used and remaining percentages for 5-hour and weekly limits, reset countdowns, Claude extra-usage spend, and Codex credits. Subscription values come from the providers' usage endpoints; token counts and API-equivalent costs come from local logs through ccusage. API-equivalent costs are separate from your subscription bill.

Use the macOS menu-bar app, native dashboard, browser dashboard, or CLI. Explore cost and token history by model, agent, machine, and directory, with local and SSH-connected machines. The CLI and dashboard server also run on Linux.

It's free and MIT-licensed. Install ccusage and the macOS app with Homebrew:

```sh
brew install ccusage
brew install --cask tacogips/tap/ccusage-gauge
```

The dashboard screenshots use example data. The menu-bar image is an actual macOS capture. I'd appreciate feedback on subscription visibility, the menu-bar experience, and which usage views would help your workflow.

## Verified result

Product Hunt displayed "Changes saved". The product preview confirmed the new
monochrome icon, tagline, description, four gallery images, refreshed maker
comment, Free pricing, and Yusuke Kawada as maker. The launch is now scheduled for November 15, 2026 at 12:01 a.m. Pacific (5:01 p.m. JST).
Gallery order is subscription detail, dark filtered view, then light overview, followed by the macOS menu bar.
Existing launch tags were retained: Open Source, Developer Tools, GitHub, Menu Bar Apps.
The existing GitHub website/repository links were retained. No shoutouts, video,
or interactive demo were added. The user subsequently requested immediate publication; the earliest available date was confirmed.

## Native menu-bar addition

`04-macos-menu-bar.png` (1270 × 760) presents the user-provided cropped
macOS menu-bar screenshot, captured at 09:02 on 2026-11-15.
`menu-bar-source.png` preserves that original. The original capture is scaled
inside a presentation frame; its icons and displayed cost are unchanged.
The isolated E2E app was closed without changing production app settings.

## Launch submission

On the user's explicit request to publish now, the earliest date offered by
Product Hunt was confirmed: Sunday, November 15, 2026 at 12:01 a.m. Pacific,
or 5:01 p.m. JST on the same date. The visible UI did not offer immediate
publication. The pre-launch dashboard confirmed an active countdown
(7 hours 48 minutes when checked). Status: scheduled, not yet live.
