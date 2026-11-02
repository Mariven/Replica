# Dashboard style

The dashboard's look is defined in `src/dashboard/style.css`, with its colors, fonts, and filters set as variables at the top of the file. This note explains how those values fit together, so that a new element can be made to match.


## Color

The neutral colors are all mixes of two axes, a foreground color and a background color, which are black and white in the light theme. Each neutral sits at a fixed point between them, measured in sixteenths. Hard text is nearly pure foreground, light text is a little past halfway, and light borders are mostly background. The dark theme swaps the axes and adjusts a few blend ratios, and the neutrals follow.

Saturated hues carry meaning: green for good, yellow for a warning, red for bad, blue for information and links, and gray for anything neutral. An element with a meaning sets `--role-color` to one of these and takes its shades from it. Its text is the hue mixed toward the foreground, and tags and labels put a pale tint of the hue behind that text. A new element should take a role this way instead of introducing a color of its own.


## Surfaces

The page background is a faint lattice of dots. Sections and their headers are transparent, with backdrop filters that saturate the texture behind them, so a section stands out from the page by a change of tint more than by its border. Headers use a stronger filter than the content below them. Sections have square corners. The status strip is the one floating element, with rounded corners and a small shadow. Tags and controls are slightly rounded.


## Type

Text is set in Roboto where it is installed, then Helvetica, at 13px. No web font is loaded, since the dashboard makes no outside requests. Paths, hashes, plans, tags, and the small control buttons use Menlo. Headings are set apart by weight and a modest size step, and spacing is kept tight, in steps of a few pixels.
