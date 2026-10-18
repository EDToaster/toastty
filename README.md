# pr-media

Before/after recordings embedded in PR descriptions. Not meant to be merged.

Each `<branch>/` folder holds `*-before.gif` (main @ f50f7f5) and `*-after.gif` (that branch) pairs.
Recorded on Linux under Xvfb (llvmpipe Vulkan) with real X input via `xdotool`; clipboard via `xclip`.
`scripts/` contains the recording harness (`demo.py`), the byte viewer (`showkeys.py`), the clipboard
viewer (`showclip`) and the per-PR demo scripts.
