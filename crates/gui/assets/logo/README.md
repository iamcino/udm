# UDM logo (Usagi theme)

Original vector artwork for UDM (Universal Download Manager): a round, soft
cream rabbit seen from behind, mid-hop with little arms up and one ear
flopping, and the white cloud-like tail as the accent. All outlines are smooth
curves. Layout follows the Yeager/cino-ui `AccentLogo` style
(rounded tile, single mark, 45deg ombre from 18.75% to 81.25%).

| File | Use |
|---|---|
| `udm-logo.svg` | Main app icon, dark-brown tile (source of `../icon*.png`, `../icon.ico` 40-256 px) |
| `udm-logo-small.svg` | Simplified tile for 16-32 px (bigger mark, scalloped tail, no fur strokes on the tail) |
| `udm-logo-light.svg` | Light variant: cream tile, brown outline |
| `udm-glyph.svg` | Tile-less mark with brown outline, readable on dark and light taskbars (source of `../tray/`) |
| `udm-glyph-mono.svg` | Single-colour mark (`currentColor`), tail masked out |

Palette: cream `#FBEFC0` (ombre `#F6DF94` -> `#FFFAE8`), outline/tile brown
`#3B2A20`, blush pink `#F4A7B6`, tail white `#FFFFFF`.

Mark silhouette for `AccentLogo.commands` (viewBox 0 0 512 512, nonzero fill;
the SVGs draw it rotated -6deg around 256,310):

```
M128 300C100 288 76 262 80 240C84 218 110 222 128 244C140 258 146 276 146 290ZM384 300C412 288 436 262 432 240C428 218 402 222 384 244C372 258 366 276 366 290ZM160 436C160 418 182 408 206 410C232 412 246 426 242 442C238 458 214 464 192 460C172 456 160 450 160 436ZM352 436C352 418 330 408 306 410C280 412 266 426 270 442C274 458 298 464 320 460C340 456 352 450 352 436ZM200 226C186 186 170 128 172 96C174 62 214 52 228 84C240 112 244 168 246 212ZM272 212C276 170 284 132 300 110C318 84 356 70 378 86C398 102 384 128 358 134C336 140 324 168 318 222ZM256 180C338 180 398 236 406 316C414 396 352 444 256 444C160 444 98 396 106 316C114 236 174 180 256 180Z
```

The PNG/ICO files were rendered from these SVGs at each target size (not
downscaled), with `udm-logo-small.svg` used for 16-32 px.
