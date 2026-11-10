# Bundled wallpapers

RAST ships with four calm nature scenes used as desktop wallpapers. They are
part of the application assets, so no external hosting or network access is
required at runtime.

| Id | Label | File | Description |
| --- | --- | --- | --- |
| `alpine-dawn` | Alpine Dawn | `frontend/public/wallpapers/alpine-dawn.jpg` | Calm lake below snow-capped peaks at sunrise |
| `forest-mist` | Forest Mist | `frontend/public/wallpapers/forest-mist.jpg` | Pine forest with soft morning light rays |
| `coastal-calm` | Coastal Calm | `frontend/public/wallpapers/coastal-calm.jpg` | Turquoise water, smooth rocks and a pale sky |
| `desert-dusk` | Desert Dusk | `frontend/public/wallpapers/desert-dusk.jpg` | Warm dunes under a violet evening sky |

## Licence

These four images were generated for the RAST project with an AI image model on
2026-11-10 and contain no third-party photographs, trademarks or people. The
RAST authors dedicate them to the public domain under
[CC0 1.0 Universal](https://creativecommons.org/publicdomain/zero/1.0/)
(`CC0-1.0`). They may be redistributed with the project and reused freely.

Files are stored as 1920×1080 JPEG (quality ~85, 110–253 KiB each) so the whole
set adds well under 1 MiB to the binary.

## User-uploaded wallpapers

Uploaded wallpapers are **not** part of the repository or the binary: they are
validated (JPEG/PNG/WebP, ≤10 MiB, reasonable dimensions), stored on the server
under `<data_dir>/wallpapers/<user-id>/`, and served only to their owner through
an authenticated endpoint. The images remain the property of the user who
uploaded them; RAST only stores and displays them for that account.

## Adding a bundled wallpaper

1. Add the image to `frontend/public/wallpapers/` (name it `<id>.jpg|png|webp`).
2. Record it in `BUILTIN_WALLPAPERS` in `frontend/src/core/appearance.ts` and in
   the backend whitelist (`src/appearance/mod.rs`).
3. Add a row to the table above with its licence information.
4. Keep files ≤ 400 KiB and 1920×1080 for a predictable binary size.
