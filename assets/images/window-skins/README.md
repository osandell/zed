# Arcoscope-linked Zed themes

Zed follows `barTheme` in `~/.config/arcoscope/gui-settings.json` every 1.5 seconds.
Defaults are declared in `arcoscope.json`. Dreamweb supplies an ordinary Zed color
and syntax theme plus original bitmap artwork designed for Zed, using Arcoscope as a style reference.
Flat restores the user's usual Zed theme; Amiga retains the existing Amiga chrome.

Override or add mappings in `~/.config/zed/arcoscope-themes.json` (strict JSON):

```json
{
  "enabled": true,
  "themes": {
    "my-next-arcoscope-theme": { "theme": "My Zed Theme" },
    "flat": { "theme": "Gruvbox Dark" }
  }
}
```

Install ordinary color themes in `~/.config/zed/themes/`. A binding's `theme`
may be null to retain the configured light/dark theme. Disabling the integration
or selecting an unmapped Arcoscope name restores normal Zed theme selection. User
settings are never rewritten. Missing themes are retried after registration;
invalid JSON preserves the last applied mapping and logs an error.

## Bitmap skin format

A binding may include `skin`. Copy the Dreamweb binding as a starting point.
Neither the renderer nor the UI components contain Dreamweb-specific colors,
source coordinates or theme-name checks.

- `image`: `asset:images/...` for a bundled asset, an absolute file path, or a
  path relative to `~/.config/zed` for personal skins.
- `reference_width`: width of the coordinate system used to author source
  rectangles. The renderer converts to the actual PNG dimensions before cropping.
- `scale`: logical UI points per reference-image pixel.
- `surfaces`: named surfaces: `workspace`, `title_bar`, `tab_bar`, `tab_active`,
  `tab_inactive`, `status_bar`, and `bottom_strip`. Omitted surfaces use standard
  Zed rendering.
- Each surface optionally has `fill` (a source rectangle with `x`, `y`, `width`,
  `height`) and `fill_mode` (`stretch`, `tile`, or `horizontal`). Horizontal mode
  repeats a vertical strip sideways, retaining its complete vertical shading.
- `frame` has a source `rect` and `borders` in **top, right, bottom, left** order,
  measured in reference pixels. It samples eight perimeter pieces, never the
  original centre. Corners retain their dimensions; long rails repeat.
- `padding` is **top, right, bottom, left** in logical UI points. The workspace
  uses it to reserve space for the outer frame without covering text or controls.

## Vector layers

A surface may instead, or on top of its bitmap parts, list `layers`: rounded
rectangles painted in order. A skin whose surfaces are all layers needs no
`image`, `reference_width` or `scale`; Mist is built this way.

```json
"terminal_window": {
  "layers": [
    { "inset": [0, 5, 0, 0], "fill": "#e4e3de", "border": "#b5bab9", "radius": [9, 9, 9, 9] },
    { "inset": [6, 11, 6, 6], "fill": "#f4f2eb", "border": "#d0d1cb", "radius": [6, 6, 6, 6],
      "etch": "#fbfaf7e6" }
  ],
  "padding": [8, 13, 8, 8]
}
```

- `inset`: **top, right, bottom, left** in points from the surface's bounds.
- `fill`, `border`, `etch`: `#rrggbb` or `#rrggbbaa`. `etch` is a 1 pt line of its
  own just outside the border.
- `border_widths`: **top, right, bottom, left**; 1 pt all round when omitted, so
  `[0, 0, 1, 0]` is a rule along the bottom.
- `radius`: top-left, top-right, bottom-right, bottom-left.

Two surfaces frame whole columns: `terminal_window` round the terminal column and
`editor_window` round the editor side (panes, docks and status bar). Their
`padding` keeps the content inside the frame, and insets on the facing sides
leave a strip of `workspace` between the two.

Use text-free patches: filenames, status glyphs, close buttons and editor text
are rendered live above the artwork. Bitmaps are decoded and cropped only when
a mapping changes; GPUI caches their GPU images. Bitmap layers have no input
handlers and preserve the existing tab interactions.

`dreamweb-zed-v3.png` is the user's chosen Zed reference cleaned with the built-in
image generation tool: all text and symbols were removed while retaining the
panel artwork. The final prompt is `dreamweb-zed-v3.prompt.txt`.

Each live panel samples its own frame and interior from this atlas. `edge_mode`
in a frame accepts the same modes as `fill_mode`; omitted preserves legacy
repeating rails. Dreamweb uses `stretch` to draw each rail once, retaining fixed
corners. There are no repeating panel tiles.

Additional surface roles: `terminal_panel`, `session_panel`, `editor_panel`,
`editor_background`, `dock_panel`, and `button`. The terminal panel's presence
also enables theme colors and a transparent Ghostty background, exposing its
bitmap interior. These are temporary in-memory config overrides; selecting a
plain theme restores the user's Ghostty config. User config files are never
rewritten. Text, selection, cursor, tabs and navigation remain live controls.

To preview artwork without recompiling, override the binding with an absolute
image path in `~/.config/zed/arcoscope-themes.json`. Use a new image filename when
changing pixels so the mapping watcher invalidates the bitmap cache.
