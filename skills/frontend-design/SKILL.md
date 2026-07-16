---
name: frontend-design
description: Concrete typography, spacing, color, and layout rules for building a UI that reads as deliberately designed instead of generic-template, plus the critique checklist to run against a ui_screenshot before shipping. Use whenever a build has a UI, right after the first visual milestone.
---

# frontend-design

Vocabulary and rules for making a prototype UI look designed, not
default. Load this the moment `web-app-scaffold` gets a dev server up,
before the first `ui_screenshot` — and again every time you look at one.
Five decisions cover 90% of "why does this look generic": type, space,
color, layout, and knowing which reflexes to refuse.

## Typography

Pick **one pairing** per app from the baked fonts, not a font-of-the-day:
Inter for UI text (labels, body, nav), JetBrains Mono for anything
data/code-shaped (numbers in a table, timestamps, code blocks, IDs).
Never pair two body-text fonts — one UI face, one data face, done.

Use a real scale, not eleven ad-hoc sizes: pick a base (16px) and a
~1.25 ratio, then **4-5 stops** — e.g. 13 / 16 / 20 / 25 / 32. Every
size on screen is one of those five. Headings get the top 2 stops,
body the middle, captions/meta the bottom.

- **Line-height**: ~1.5 for body copy, ~1.2 for headings (tighter as
  size grows — a 32px heading at 1.5 looks loose, not readable).
- **Measure**: cap body text at ~65-75 characters per line
  (`max-width: 65ch` on the text container). Full-viewport-width prose
  is unreadable, not "clean."

## Spacing

Pick **one unit** — 4px or 8px — and make every margin, padding, and
gap a multiple of it. No 15px, no 22px. This alone kills the "someone
eyeballed every value" look.

Whitespace is a feature, not what's left over: when in doubt, add more
space around the thing you want the eye to land on, not more border or
more color. Group by proximity — elements that relate sit close
together with a small gap; unrelated groups get a visibly larger gap
between them, not the same gap as everything else. If every element is
equidistant, there is no hierarchy, only a grid of boxes.

## Color

**One saturated accent color** plus a neutral ramp (5-7 steps of gray
from near-white to near-black). The accent marks the one or two things
per screen that matter (primary action, active state, key data point) —
if everything is the accent color, nothing is.

Derive states from the accent instead of picking new colors: hover =
accent shifted ~10% darker/lighter, disabled = accent at low opacity
over neutral, focus ring = accent at full saturation with a visible
offset. Don't invent a second and third brand color for "success" and
"danger" without checking they don't fight the accent's hue.

Contrast floor: WCAG AA — 4.5:1 for body text, 3:1 for large text
(18px+ bold or 24px+) and UI borders/icons. Check it, don't eyeball it.
On dark backgrounds, desaturate the accent slightly (a fully saturated
accent vibrates against near-black — pull saturation down ~10-15%
before using it on a dark surface).

## Layout

**Hierarchy first**: before placing a single element, answer "what is
the ONE thing this screen is for?" — that thing gets the biggest size,
the accent color, or the top-left/top-center position (whichever the
content type expects). Everything else is secondary by construction,
not by accident.

- **Align to a grid.** Pick a column count (4 for narrow, 12 for wide)
  and a gutter; every element's edge lines up with another element's
  edge somewhere on the page. Floating elements that don't line up
  with anything read as unpolished even when each one looks fine alone.
- **Max content width.** Cap the main content column (960-1200px is a
  reasonable default) and center it — full-bleed text/forms on a wide
  monitor is a scaffold default, not a decision.
- **Design the states you'd normally skip.** Empty (zero items — say
  what to do next, don't render a blank div), loading (a skeleton or
  spinner that matches the final layout's shape, not a generic
  spinner dropped in the corner), and error (what broke + what to do,
  not a raw stack trace or a bare "Error"). A prototype that only shows
  the happy path hasn't shown you the UI, just a screenshot of luck.

## Anti-generic rules

Reflexes to catch and refuse:

- **No default-blue gradient hero.** If the first thing you'd reach
  for is a blue-to-purple gradient banner with centered white text,
  reach for something else — a real screenshot of the app's own UI, a
  strong type moment, or plain background with real content.
- **No lorem ipsum, ever.** Write real microcopy for every string —
  button labels, empty states, error text, placeholder text. Fake copy
  in a screenshot signals "unfinished" even when the layout is done.
- **No three-equal-cards-in-a-row by reflex.** That layout is correct
  for exactly one content shape (three genuinely parallel items); for
  anything else (a list, a single hero stat, a timeline) it's the
  layout you reached for because it's easy, not because it fits.
- **Pick border-radius and shadow once, reuse everywhere.** One radius
  value (e.g. 8px) and one shadow recipe for all cards/buttons/inputs —
  mixing sharp corners, 4px radii, and 16px radii on one screen reads
  as un-designed even with good spacing.
- **Tailwind (baked) with a constrained palette.** Restrict yourself to
  the type scale, spacing unit, and 1-2 colors above instead of
  reaching for the full default Tailwind color/spacing palette — an
  unconstrained utility framework produces the same "generic" result as
  no framework at all.

## Critique checklist

Run this against every `ui_screenshot`, out loud, before moving on —
this is the depth `web-app-scaffold`'s see-then-fix loop points at:

1. **Hierarchy** — does the ONE thing this screen is for jump out
   first, or does everything compete equally?
2. **Alignment** — does every element's edge line up with something
   else, or are things floating at arbitrary positions?
3. **Contrast** — does body text clear 4.5:1 against its background?
   Anything squint-worthy?
4. **Crowding** — is anything touching or too close relative to the
   spacing unit? Does related content sit closer than unrelated content?
5. **Real copy** — is there any lorem ipsum, "Lorem", or placeholder
   text still on screen?
6. **States** — have you actually looked at empty/loading/error, or
   only the happy path with data already in it?
7. **One accent** — is exactly one saturated color doing the marking
   work, or have two or three accent-strength colors crept in?
8. **Consistent radii/shadow** — is there one border-radius and one
   shadow recipe reused everywhere, or a mix?

Fix the worst two answers, screenshot again, repeat once before the
delivery todo — don't ship on the first look.

## Related skills

[[web-app-scaffold]] (the see→fix loop this checklist plugs into),
[[coding-task]] (the delivery ritual that ships the final screenshot).
