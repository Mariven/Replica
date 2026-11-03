# Dashboard style

The dashboard's look is defined in `src/dashboard/style.css`, with its colors, fonts, and filters set as variables at the top of the file. This note explains how those values fit together, so that a new element can be made to match. It also describes components the dashboard does not use yet, with the values to build them.


## Color

The neutral colors are all mixes of two axes, a foreground color and a background color, which are black and white in the light theme. Each neutral sits at a fixed point between them, measured in sixteenths. Hard text is nearly pure foreground, light text is a little past halfway, and light borders are mostly background. The dark theme swaps the axes and adjusts a few blend ratios, and the neutrals follow.

Saturated hues carry meaning: green for good, yellow for a warning, red for bad, blue for information and links, and gray for anything neutral. An element with a meaning sets `--role-color` to one of these and takes its shades from it. Its text is the hue mixed toward the foreground, and tags and labels put a pale tint of the hue behind that text. A new element should take a role this way instead of introducing a color of its own.

Two more shades come from the alpha values in the stylesheet. The shadow shade is the hue at 25% alpha. The accent shade is the hue at 6.25% alpha in the light theme and 30% in the dark theme.


## Surfaces

The page background is a faint lattice of dots. Sections and their headers are transparent, with backdrop filters that saturate the texture behind them, so a section stands out from the page by a change of tint more than by its border. Headers use a stronger filter than the content below them. Sections have square corners. The status strip is the one floating element, with rounded corners and a small shadow. Tags and controls are slightly rounded.

Corner radii depend on the kind of element:

- 0 for blocks and sections
- 2px for tags and scrollbar thumbs
- 4px for controls and code
- 5px for cards and hover outlines
- 8px for floating containers and compact fields


## Type

Text is set in Roboto where it is installed, then Helvetica, at 13px. No web font is loaded, since the dashboard makes no outside requests. Paths, hashes, plans, tags, and the small control buttons use Menlo. Headings are set apart by weight and a modest size step, and spacing is kept tight, in steps of a few pixels.

Headings inside rendered content, such as a file preview, are 1.5em at weight 600, then 1.3em and 1.2em. Their vertical margins are 6px, 4px, and 3px by level.


## Blocks

Blocks are strips of content that carry a hue, such as the entries in a list of relations or plans.

- Blocks have square corners, 4px 8px padding, and the light shade as background.
- Their side rules are in the main shade. They are 5px wide on the leading edge, 3px on the trailing edge, and 2px where two blocks meet.
- Their top and bottom rules are 1px wide and stay transparent until the block is hovered or selected.
- Result areas show the output of an action. They have a 1px rule in the main shade on all four sides, widened to 5px on the leading edge and 3px on the trailing edge.
- Separator blocks use the light text color as background.
- Block toolbars have 0.9em text over the accent shade, with their controls in the light text color.

Blocks change with their state:

- Hovered blocks are drawn above their neighbors, and their top and bottom rules turn to the main shade.
- Selected blocks have a 1px top rule in the main shade. Their toolbar gets the same top rule over a 40 degree gradient from the accent shade at 0% to the main shade at 500%, and its text turns to the medium text color.
- Blocks being edited have a top rule in the main shade, and their text turns to the heavy shade.
- Disabled blocks have 60% opacity, or 30% when they are also collapsed.
- Busy blocks have a rule in the shadow shade over the light shade. They glow with a 10px blur in the shadow shade and a 2px blur in the main shade.
- Collapsed blocks hide their content and have a 2px dashed bottom rule.
- Condensed blocks show their content in 0.85em text with a 0.9em line height, up to a fixed height. Their background fades from transparent at the top, through the light shade at 60%, to the accent shade at the bottom. They scroll with a 7px scrollbar, whose thumb is in the foreground color.
- De-emphasized blocks have 0.95em text at 60% opacity on a transparent background.


## Controls

The daemon controls in the header show the button group and the danger button. Other controls follow these rules:

- Buttons have 1em sans text, 4px 8px padding, and a 4px radius. Plain buttons have hard text on a transparent background. Buttons with a hue use it for their text and background, and switch to the heavy shade on hover.
- Action buttons, such as the edit and delete buttons on a list row, use the role's text shade. On hover, that shade is mixed halfway toward the background.
- Disabled buttons have 50% opacity and the not-allowed cursor. Disabled options in a select use the light text color.
- Text fields, text areas, and selects have 1em soft text, 8px padding, a 10px bottom margin, a 1px light border, a 4px radius, and the light main background. Inside a panel, they have 0.9em text, 2px padding, a transparent background, and no bottom margin.
- Inline fields have a 1px bottom border in the light border color as their only border.
- Compact fields have mono text, 2px padding, and an 8px radius, with a transparent background behind a backdrop filter of `saturate(750%) contrast(110%) brightness(110%)`.
- Cards have a 2px border, a 5px radius, and a backdrop filter of `saturate(250%) brightness(110%)`. Inactive cards have 70% opacity.


## Code

Code blocks have a 2px inset border in the medium text color, a 4px radius, 6px padding, and a `#fffa` background, with wrapped `#333` text at 0.9em. Syntax highlighting keeps the block's background and base text color.

Inline code has a 1px inset border in the light text color, a 4px radius, the light main background, and soft text at 0.95em.


## Lists and feedback

- List rows have 1px light borders on their bottom and left sides. On hover, a row takes the light border color as background and shows its action buttons. While the pointer is over one of those buttons, the row background changes to the button's hover shade at 50% alpha.
- Editable text shows a 1px outline with a 5px radius on hover, in the faint border color.
- Read-only entries are set in small caps in the light text color.
- Status messages are centered mono text in the pure hue of their meaning. They appear at once and fade out over 1s.
- After an action is confirmed, the leading rule of its block turns green for 300ms.
- Meters show a 2px bar in the main shade under their value, with a width proportional to the value.
- Keyboard shortcut hints appear on hover, at 0.8em in the light text color.


## Icons

Icons come from Lucide. They are line icons on a 24 unit view box, stroked in the current text color at width 2 with round caps and joins. Icons are 1em or 12px square, and an icon beside text moves down 2px to 3px to line up with it.


## Motion

Transitions use ease-in-out. Borders and backgrounds change over 0.1s, result areas over 0.15s, fades over 0.2s, and sizes and meters over 0.3s. Block backgrounds change color on a linear curve. When the system asks for reduced motion, transitions are turned off.

On screens 768px wide or less, columns stack, and text in side panels drops to 0.9em.
