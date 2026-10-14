---
name: Nuée
description: A room fills one Séyès notebook page together, square by square, in each device's ink.
colors:
  paper: "#fbfcff"
  field: "#ffffff"
  rule-minor: "#e4e8f7"
  rule-major: "#c3cbec"
  margin-line: "#e2546b"
  ink: "#16213d"
  pencil: "#4b5468"
  border: "#7c86a6"
  accent: "#1d3b9a"
  accent-hover: "#142c78"
  accent-text: "#ffffff"
  red: "#c62e3a"
  green: "#1e7a4c"
  ochre: "#8a5a00"
  focus: "#b35c00"
  selection: "#d6ddf7"
  on-cover: "#ffffff"
  cover-slate: "#3d4a5c"
  cover-blue: "#1d3b9a"
  cover-green: "#1e6b47"
  cover-ochre: "#7a5100"
  cover-teal: "#136b78"
  cover-violet: "#5b3a9e"
  ink-0: "#1d3b9a"
  ink-1: "#6a3fa0"
  ink-2: "#12808f"
  ink-3: "#1e7a4c"
  ink-4: "#8a5a2b"
  ink-5: "#1a1a2e"
  ink-6: "#5c6b12"
  ink-7: "#44506b"
  board-paper: "#18241f"
  board-field: "#101a16"
  board-ink: "#ecefe6"
  board-pencil: "#b9c3b6"
  board-accent: "#a9c6ff"
  board-red: "#ff9e96"
  board-margin-line: "#e98b98"
typography:
  display:
    fontFamily: "Luciole, system-ui, -apple-system, 'Segoe UI', sans-serif"
    fontSize: "clamp(3rem, 9vw, 5.5rem)"
    fontWeight: 700
    lineHeight: 1
    letterSpacing: "-0.03em"
  headline:
    fontFamily: "Luciole, system-ui, sans-serif"
    fontSize: "2.125rem"
    fontWeight: 700
    lineHeight: "2.5rem"
    letterSpacing: "-0.015em"
  title:
    fontFamily: "Luciole, system-ui, sans-serif"
    fontSize: "1.4375rem"
    fontWeight: 700
    lineHeight: "2rem"
    letterSpacing: "-0.01em"
  subtitle:
    fontFamily: "Luciole, system-ui, sans-serif"
    fontSize: "1.1875rem"
    fontWeight: 700
    lineHeight: "1.5rem"
  figure:
    fontFamily: "Luciole, system-ui, sans-serif"
    fontSize: "1.75rem"
    fontWeight: 700
    lineHeight: "2rem"
    fontFeature: "tnum"
  body:
    fontFamily: "Luciole, system-ui, sans-serif"
    fontSize: "1rem"
    fontWeight: 400
    lineHeight: "1.5rem"
  label:
    fontFamily: "Luciole, system-ui, sans-serif"
    fontSize: "0.875rem"
    fontWeight: 400
    lineHeight: "1.25rem"
  task-number:
    fontFamily: "'JetBrains Mono', ui-monospace, Menlo, monospace"
    fontSize: "0.625rem"
    fontWeight: 400
    lineHeight: 1
    letterSpacing: "-0.04em"
    fontFeature: "tnum"
  emoji:
    fontFamily: "'Apple Color Emoji', 'Segoe UI Emoji', 'Noto Color Emoji', sans-serif"
    fontSize: "clamp(1.75rem, 8vw, 2.5rem)"
    lineHeight: 1.2
  emoji-margin:
    fontFamily: "'Apple Color Emoji', 'Segoe UI Emoji', 'Noto Color Emoji', sans-serif"
    fontSize: "2rem"
    lineHeight: 1.2
  emoji-present:
    fontFamily: "'Apple Color Emoji', 'Segoe UI Emoji', 'Noto Color Emoji', sans-serif"
    fontSize: "clamp(2.5rem, 7vw, 5rem)"
    lineHeight: 1.2
rounded:
  square: "2px"
  slip: "4px"
  sm: "6px"
  card: "12px"
spacing:
  "1": "4px"
  "2": "8px"
  "3": "16px"
  "4": "24px"
  "5": "32px"
  "6": "48px"
  "7": "64px"
components:
  button-primary:
    backgroundColor: "{colors.accent}"
    textColor: "{colors.accent-text}"
    rounded: "{rounded.sm}"
    padding: "0 16px"
    height: "44px"
  button-primary-hover:
    backgroundColor: "{colors.accent-hover}"
    textColor: "{colors.accent-text}"
  button-secondary:
    backgroundColor: "{colors.paper}"
    textColor: "{colors.accent}"
    rounded: "{rounded.sm}"
    padding: "0 16px"
    height: "44px"
  button-secondary-hover:
    backgroundColor: "{colors.selection}"
    textColor: "{colors.accent-hover}"
  button-danger:
    backgroundColor: "{colors.paper}"
    textColor: "{colors.red}"
    rounded: "{rounded.sm}"
    height: "44px"
  button-big:
    backgroundColor: "{colors.accent}"
    textColor: "{colors.accent-text}"
    rounded: "{rounded.sm}"
    height: "56px"
    width: "100%"
  input-text:
    backgroundColor: "{colors.field}"
    textColor: "{colors.ink}"
    rounded: "{rounded.sm}"
    padding: "0 8px"
    height: "44px"
  header-cover:
    backgroundColor: "{colors.cover-blue}"
    textColor: "{colors.on-cover}"
    height: "64px"
  cover-label:
    backgroundColor: "{colors.cover-green}"
    textColor: "{colors.on-cover}"
    rounded: "{rounded.sm}"
    padding: "0 16px"
    height: "48px"
  task-square:
    backgroundColor: "{colors.ink-0}"
    textColor: "{colors.paper}"
    typography: "{typography.task-number}"
    rounded: "{rounded.square}"
    size: "24px"
---

# Design System: Nuée

## Overview

**Creative North Star: "The Shared Notebook Page"**

Nuée is one French Séyès-ruled school notebook page that the whole room fills together. Every task is a square someone colours in with their own ink, every rejected result is circled in red in the margin like a teacher's correction, and every area of the app is a notebook with its own cover colour. In dark mode the page becomes the classroom blackboard: green-black slate, chalk white, chalk pastels. The ruling is the page itself, not decoration: 8px minor lines, 32px major lines, and a red margin line down the left of every body.

Density is working density (Operate mode): live figures in one tabular line, a 16×16 task grid aligned on the ruling, a device roster as a ruled table. Data is real and never staged. One hyperlegible face, Luciole, carries everything; JetBrains Mono appears only for task numbers, links and commands. The system refuses the category's grey dashboard of identical cards and hero metrics: there are no card containers with shadows, no coloured-edge alert boxes, no zebra rows.

**Key Characteristics:**
- Séyès ruling (8px / 32px) and a red margin line on every page body, in both themes.
- Two inks for meaning: fountain-pen royal blue for text, figures and the primary action; red only for corrections.
- Each area has a full-strength notebook cover colour on its header band.
- Each device writes in its own distinct ink, none of them red-adjacent.
- Flat paper; the only shadow is the paper slip under the QR code.

## Colors

A cool notebook paper with royal-blue fountain-pen ink, a red correction pen, six cover colours and eight device inks; the blackboard theme swaps each for a chalk equivalent of the same role.

### Primary
- **Fountain-Pen Royal Blue** (accent): page titles, links, live figures, progress bars, checked choices, and the filled primary button. Hover deepens to Blotted Blue (accent-hover). On the blackboard it becomes Chalk Blue (board-accent).

### Secondary
- **Correction Red** (red): only for a correction or a refusal: the circled rejected square and its tick, the margin correction note, banned roster rows, error alerts, a figure that counts mismatches, and the outlined destructive button. Chalk Coral (board-red) on the blackboard.
- **Margin Rose** (margin-line): the 2px vertical margin line, nothing else.

### Tertiary
- **Notebook covers** (cover-slate, cover-blue, cover-green, cover-ochre, cover-teal, cover-violet): the header band of each area at full strength with white lettering (on-cover, above 4.5:1). Launch and run are blue, Join green, Share and file ochre, Site teal, Train violet, About and Home slate. Darker covers on the blackboard.
- **Device inks** (ink-0 to ink-7): royal blue, violet, teal, green, brown, near-black, olive, slate. One per device, assigned in order, used for filled task squares, roster swatches and murmuration birds. Chalk pastels on the blackboard.
- **Verified Green** (green) and **Notice Ochre** (ochre): confirmation badges, and notice or waiting lines that are not errors.

### Neutral
- **Notebook Paper** (paper): page background; **White Slip** (field): fields and command slips.
- **Minor Rule** (rule-minor) and **Major Rule** (rule-major): the ruling, and every divider in lists, tables and fieldsets.
- **Ink** (ink): body text; **Pencil** (pencil): secondary text, labels, hints, empty states; **Border** (border): field and square outlines, at least 3:1 on paper.
- **Focus Amber** (focus): the 3px focus ring. **Selection Wash** (selection): text selection and secondary hover.
- Blackboard: Slate (board-paper), Deep Slate (board-field), Chalk (board-ink), Chalk Dust (board-pencil).

### Named Rules
**The Two-Ink Rule.** Blue ink carries text, figures and the primary action; red appears only for a correction. A count is blue until it counts something wrong.

**The Distinct Inks Rule.** Devices write in distinct inks, and no device ink may be red or red-adjacent. Red belongs to the teacher.

**The Cover Rule.** Each destination wears its full-strength cover colour as its header band and as its door label on Home. Home is the shelf: its band is slate, shared with About.

## Typography

**Body Font:** Luciole (with system-ui, -apple-system, Segoe UI, sans-serif), regular, italic, bold, bold italic, self-hosted, CC BY 4.0.
**Label/Mono Font:** JetBrains Mono (with ui-monospace, SF Mono, Menlo, Consolas), regular only.

**Character:** One hyperlegible French face for everything, bold for headings and figures, regular for reading. Mono is a tool, used where characters must be compared exactly.

### Hierarchy
- **Display** (700, clamp(3rem, 9vw, 5.5rem), line-height 1): the Home hero title only.
- **Headline** (700, 34px, 40px): page titles (h1), in royal-blue ink; the presenter's count.
- **Title** (700, 23px, 32px): section headings (h2), legends, cover labels, the wordmark.
- **Subtitle** (700, 19px, 24px): h3 and margin-column headings; Lead text is 19px regular on a 32px line in pencil.
- **Figure** (700, 28px, 32px, tabular): live statistics.
- **Body** (400, 16px, 24px): paragraphs, capped at 70ch.
- **Label** (400, 14px): stat labels, hints, footer, task detail.
- **Task number** (JetBrains Mono 400, 10px, or 8px in a dense grid, tabular): the number inside each task square.
- **Fingerprint symbols** (the system emoji face; `--text-emoji` clamp(1.75rem, 8vw, 2.5rem) in a page, `--text-emoji-margin` 2rem in the presenter's margin, `--text-emoji-present` clamp(2.5rem, 7vw, 5rem) in presentation mode): the six symbols, one row when they fit, three over three when they do not.

### Named Rules
**The Ruled Line Rule.** Every line height is a multiple of the 8px ruling (24, 32, 40px); type sits on the notebook lines.

**The Tabular Figures Rule.** Every live number (figures, counts, times, roster cells, number inputs) uses tabular figures so values do not jitter as they change.

## Layout

The page is a ruled notebook page with a margin. The body has a red margin line at `--margin` (20px on phones, 48px from 40rem, 240px from 64rem) and content starts one gutter (16, 24, 32px) to its right. Content is capped at 72rem, prose at 70ch. Spacing steps are 4, 8, 16, 24, 32, 48, 64px.

On the presenter run view at 64rem and above, the margin is a real column: the QR slip, the link, Copy and Present, the six fingerprint symbols, and the red corrections live in it; right of the margin sit the title, one line of live figures, the task grid (max 32rem, 32px pitch with a `--snap` offset that lands squares on the page ruling) and the device roster. Primary actions align bottom right.

On phones the header splits into brand and language on one row and the areas as three equal tabs below; stats become a two-column grid; the device roster drops its fixed columns and writes each device as a ruled block (symbols and state, then type, done and verified under small labels); the join view is a single column ending in one full-width Contribute button. Nothing scrolls sideways at 320px. Touch targets are at least 44px.

In reading order the presenter's work column (the job name first) comes before the margin column; on wide screens the grid places the margin column to its left. After a main action (Start, Contribute, Stop, Accept or Refuse) focus moves to the new state's heading or status, or to the next device waiting for an answer.

## Elevation & Depth

Flat. Depth is the notebook itself: paper, ruling, slips of white paper. There are no card shadows and no layered surfaces.

### Shadow Vocabulary
- **Paper slip** (`box-shadow: 0 1px 2px rgb(22 33 61 / 0.12), 0 6px 16px rgb(22 33 61 / 0.08)`; darker on the blackboard): only under the QR code, which sits on a white slip in both themes so phones can always read it.

### Named Rules
**The One Slip Rule.** Only the QR code lifts off the page. Everything else is written on it.

## Shapes

Small, practical corners: 6px (`--radius`) on buttons, fields, alerts and cover labels; 4px (`--radius-slip`) on the QR slip; 2px (`--radius-square`) on task squares, swatches and progress bars. Translucent whites on a cover (hover wash, tab rule, label frame) are mixed from `--on-cover`. Dividers are 1px major-rule lines, never boxes. Cover labels on Home carry a thin inner frame (1px white at 55%, inset 4px), like the label on a notebook cover. The one irregular shape is the correction ring: a hand-drawn 2px red oval (uneven radii, rotated -8°) slightly larger than the square, staying inside the 8px gap.

## Components

### Buttons
Ink on paper, decisive and plain.
- **Shape:** gently rounded (6px), at least 44px tall, bold label, 2px-stroke icon.
- **Primary:** filled royal blue with white text; hover deepens; press scales to 0.96 (except the big button).
- **Secondary:** paper fill, blue outline and text; hover washes with selection blue.
- **Danger:** paper fill, red outline and text; hover fills red.
- **Big:** full width, 56px tall, 19px label: the one Contribute action on a phone.
- **Disabled:** 45% opacity, no hover change.

### Inputs / Fields
- **Style:** white slips on the ruling: 1px border, 6px radius, 44px tall, inherited type. Labels are bold above the field. Checkboxes and radios are 22px in accent blue.
- **Focus:** a 3px amber outline at 2px offset, everywhere in the app.

### Navigation
The header is the notebook cover of the current area: a 64px band in the area's cover colour, white wordmark, white nav links underlined on hover (1px) and for the current page (2px, bold), and an outlined language toggle. Focus rings turn white on the cover.

### Lists, Tables and Messages
Choices, doors, pending items and table rows are ruled lines with 1px major-rule dividers and no zebra; table headers are 14px pencil over a blue rule. Alerts and notices are written on the page: a 1px currentColor outline, paper fill, red for errors and ochre for notices, never a coloured-edge box. An alert is only for a refusal, a ban or a failure; a person's own stop is plain text and a full or ended swarm is a notice. A field's own problem is a red line under it, tied with `aria-describedby` and `aria-invalid`. The corrections note is in pencil while nothing was corrected and turns red with the first correction.

### Presentation
Present full screen opens a modal `<dialog>` (focus stays inside; Escape or Close returns focus to the button) whose ruled stage goes full screen: the QR huge on its slip, the six symbols, the link and the live count.

### Task Grid (signature)
The page that fills. One 24px square per task on the 32px ruling, outlined in border grey with its number in mono. When a device returns a result the square fills with that device's ink and inks in over 180ms (scale 0.92 to 1); a settled grid does not animate. A failed square is dashed. A rejected result is circled in red, and the circle stays after the task is redone and verified, with the honest device's ink inside and a small red tick at its corner: the teacher's mark.

### Murmuration (signature)
A canvas flock on Home, each bird drawn in a device ink read from the page. It flies for about five seconds of visible time, glides back onto the still silhouette over the last 1.5s and stays there; it pauses when hidden or out of view and is drawn once, still, under reduced motion. Nothing on the page moves for longer than five seconds.

## Do's and Don'ts

### Do:
- **Do** draw the Séyès ruling (8px minor, 32px major) and the red margin line on every page body; it is the material of the world, including repeating stripe gradients.
- **Do** keep text, figures and the primary action in royal-blue ink, and use red only for corrections and refusals.
- **Do** give each new device the next distinct ink in order, and keep any new ink away from red.
- **Do** put each destination's full-strength cover colour on its header band; keep Home and About on slate.
- **Do** keep a rejected square's red circle after it is corrected.
- **Do** use tabular figures for every live number and line heights on the 8px ruling.
- **Do** limit authored motion to the 180ms task ink-in and the Home murmuration, both static under prefers-reduced-motion; state transitions are 150ms ease-out and drop to 0ms.

### Don't:
- **Don't** build grey dashboards of identical cards with hero metrics.
- **Don't** use red, or a red-adjacent hue, for a device ink, a decoration or a neutral state.
- **Don't** add shadows beyond the QR paper slip.
- **Don't** frame messages as boxes with a coloured edge; write them on the page.
- **Don't** add a second text face; mono is only for task numbers, links and commands.
