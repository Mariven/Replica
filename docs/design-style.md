# Design style

The dashboard pages follow this style.

## Principles

- Neutral colors come from a foreground axis and a background axis. Each neutral mixes the axes at a fixed ratio in sixteenths.
- Saturated primary hues mark categories and meanings. Text, fills, rules, and glows use fixed mixes of those hues toward an axis.
- Pages have a faint textured image as background. Panels over it are transparent, with backdrop filters that saturate the texture behind them.
- Values and compact controls use mono text. Everything else uses sans text.
- Blocks use tight padding in small pixel steps.

## Type

- Sans: Roboto when installed locally, then Helvetica, then the system sans-serif. No web font is loaded: the dashboard is a loopback tool and makes no external requests.
- Mono: Menlo, then Monaco, then Courier New, then the system monospace.
- Compact mono: Courier New, then the system monospace.
- Base size: 13px.

Sizes relative to the base:

- Top-level headings inside content: 1.5em at weight 600
- Second-level headings inside content: 1.3em
- Third-level headings inside content: 1.2em
- Page and panel headings: 1.2em at weight 400 in the header text color
- Mono option rows and de-emphasized content: 0.95em
- Mono values and compact controls: 0.9em
- Condensed content: 0.85em with a 0.9em line height
- Notes and shortcut hints: 0.8em

Headings inside content have vertical margins of 6px, 4px, and 3px by level. Page and panel headings have a 10px bottom margin.

## Color

### Axes

- Foreground axis: #000
- Background axis: #fff
- Main background: #fff

Derived colors mix a base color with a percentage of an axis in sRGB.

### Neutrals

Each neutral mixes the foreground axis with a percentage of the background axis.

- Strong border: 0%
- Hard text: 6.25%
- Hover outline: 12.5%
- Soft text and header text: 18.75%
- Medium text: 31.25%
- Light text: 56.25%
- Light border, container shadow, and inline rule: 87.5%

The light main background mixes the main background with 25% of the background axis.

### Hues

- Red: #f00
- Green: #0f0
- Blue: #00f
- Yellow: #ff0
- Magenta: #b0b
- Gray: #777
- Near-black: #222

Semantic meanings:

- Bad: red
- Good: green
- Info: blue
- Highlight: yellow

Categories take any hue from the list.

### Hue shades

Each element with a hue derives these shades from it:

- Main: the hue
- Heavy: the hue mixed with the heavy blend ratio of the foreground axis, 30% by default
- Light: the hue mixed with the light blend ratio of the background axis, 95% by default
- Shadow: the hue at the shadow alpha, 0.25 by default
- Accent: the hue at the accent alpha, 0.0625 by default
- Action text: the hue mixed with 25% of the foreground axis
- Action hover: the action text color mixed with 50% of the background axis
- Token text: the hue mixed with 50% of the foreground axis
- Token background: the hue mixed with 93.75% of the background axis

## Texture

The page background uses fixed layers that fill the viewport behind the content.

- The lower layer has the main background color with an image under opacity(30%).
- The upper layer has a color or a gradient over the image.

By default, the image shows a dot lattice rotated 45 degrees on 80px tiles. The lattice has pale blue-gray dots (#e2e6f0, #e2e8f0) with white dots on a #edf0f5 fill.

## Surfaces

- Floating containers use the main background color with an 8px radius and a 0 2px 4px shadow in the container shadow color.
- Panels have a transparent background with a backdrop filter.
- Open sections have square corners with a 1px outset border in the background axis color.
- Section headers have 5px padding, a 1px outset border, and the section header backdrop filter.

Backdrop filters:

- Section header: saturate(800%) brightness(100%) contrast(98%) blur(0px)
- Section header on hover: saturate(500%) contrast(98%) blur(2px)
- Section content: saturate(200%) opacity(1)
- Card: saturate(250%) brightness(110%)
- Button: saturate(250%) brightness(100%)
- Button on hover: saturate(750%) brightness(100%)
- Compact field: saturate(750%) contrast(110%) brightness(110%)

## Scales

Spacing steps: 1px, 2px, 3px, 4px, 5px, 6px, 8px, and 10px.

Corner radii:

- Blocks and sections: 0
- Tokens and scrollbar thumbs: 2px
- Controls and code: 4px
- Cards and hover outlines: 5px
- Floating containers and compact fields: 8px

Durations:

- Buttons: instant
- Borders, backgrounds, and section headers: 0.1s
- Result areas: 0.15s
- Fades: 0.2s
- Heights, widths, and meters: 0.3s
- Status messages: 1s

Transitions use ease-in-out. Block backgrounds change color with linear timing. Under a reduced-motion preference, transitions turn off.

## Hued blocks

- Hued blocks have square corners with the light shade as background and 4px 8px padding.
- Their side rules use the main shade at 5px on the leading edge, 3px on the trailing edge, and 2px where blocks meet.
- Their top and bottom rules are 1px wide. These rules stay transparent until hover or selection.
- Result areas use a 1px rule in the main shade on every side, widened to 5px on the leading edge and 3px on the trailing edge.
- Separator blocks use the light text color as background.
- Block toolbars use 0.9em text over the accent shade. Toolbar controls use the light text color.

## States

- On hover, a block gets a higher z-index than its neighbors. Its top and bottom rules switch to the main shade.
- Selected blocks use the light shade as background with a 1px top rule in the main shade. Their toolbar gets a top rule in the main shade, over a 40 degree gradient from the accent shade at 0% to the main shade at 500%. Toolbar text in a selected block uses the medium text color.
- In editing mode, a block has a top rule in the main shade. Its text uses the heavy shade.
- Disabled blocks have 60% opacity. Disabled blocks that are also collapsed have 30% opacity.
- Busy blocks have a rule in the shadow shade with the light shade as background. Their glow is 0 0 10px in the shadow shade with 0 0 2px in the main shade.
- Collapsed blocks hide their content. Their bottom rule is 2px dashed.
- Condensed blocks use condensed text at a capped height. Their background fades from transparent at the top to the light shade at 60% and the accent shade at the bottom. They scroll with a 7px scrollbar, where the thumb has a 2px radius in the foreground axis color.
- De-emphasized blocks use 0.95em text at 60% opacity on a transparent background.

## Controls

- Buttons use 1em sans text with 4px 8px padding, a 4px radius, and a pointer cursor. Default buttons have hard text on a transparent background. Hued buttons use their hue for text and background. On hover, hued buttons switch to the heavy variants.
- Primary button groups have a 1px solid gray frame with a 4px radius. Their buttons use mono 0.9em text, 0 8px padding, 2px margins, and the button backdrop filter. On hover, their text is bold under the hover filter.
- Action buttons use the action text shade of the hue for their meaning. On hover, they use the action hover shade.
- Danger buttons have red text with a red border. Their background mixes red with 6.25% of the background axis at 50% alpha. On hover, the text uses the hard text color, while the background mixes red with 50% of the background axis at 50% alpha.
- Disabled buttons have 50% opacity with the `not-allowed` cursor. Disabled options use the light text color.
- Inputs, text areas, and selects use 1em soft text, 8px padding, a 10px bottom margin, a 1px light border, a 4px radius, and the light main background.
- Inputs inside panels have a transparent background, 0.9em text, 2px padding, and zero bottom margin.
- Inline inputs have a 1px bottom border in the inline rule color as their only border.
- Compact fields use mono text with 2px padding, a transparent background, the compact field backdrop filter, and an 8px radius.
- Cards use a 2px border with a 5px radius and the card backdrop filter. Inactive cards have 70% opacity.

## Code and tokens

- Code blocks use a 2px inset border in the medium text color, a 4px radius, 6px padding, a #fffa background, and wrapped #333 text at 0.9em.
- Inline code uses a 1px inset border in the light text color, a 4px radius, the light main background, and soft text at 0.95em.
- Syntax highlighting inherits the background and base text color of its code block.
- Inline tokens use mono text at 0.9em with 0 2px padding and a 2px radius. Their colors are the token text and token background shades of their hue. The default token hue is red.

## Lists and feedback

- List rows have 1px light borders on the bottom and the left. On hover, a row uses the light border color as background, with its action buttons visible. While the pointer is over an action button, the row background uses that button's action hover shade at 50% alpha.
- Editable text shows a 1px outline in the hover outline color with a 5px radius on hover.
- Read-only entries use small caps in the light text color.
- Status messages use centered mono text in the pure hue of their meaning. They appear at once, then fade out over 1s with ease-in.
- After a confirmed action, the leading rule of the block turns green for 300ms.
- Meters show a 2px bar in the main shade under their value. The width of the bar is proportional to the value.
- Keyboard shortcut hints appear on hover at 0.8em in the light text color.

## Icons

- Icons come from Lucide as line icons on a 24 unit view box.
- Strokes use the current text color at width 2 with round caps and joins.
- Icons measure 1em or 12px. Icons beside text shift down 2px to 3px to line up with the text.

## Narrow screens

At 768px wide or less, columns stack. Text in side panels uses 0.9em.

## Dark theme

- Foreground axis: #fff
- Background axis: #111
- Main background: #101010
- Accent alpha: 0.3
- Heavy blend ratio: 70%
- Light blend ratio: 85%

The dark theme uses its own background image.
