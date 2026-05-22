# Text-rendering snapshot goldens

Each `text_snapshot_*.png` here is a reference output from the headless
text-render harness in the sibling `tests/` directory. They are
**committed PNGs**, not generated artifacts.

## How comparisons work

Each test:

1. Builds a `Term`, feeds it a fixed input sequence.
2. Renders it offscreen with `Renderer`'s text pipeline.
3. Reads the framebuffer back into an `image::RgbaImage`.
4. Loads the golden PNG from this directory and compares via
   `image-compare`'s `Algorithm::MSSIMSimple` over the RGB channels.
5. Requires SSIM ≥ 0.99.

## First-run / regeneration

Set the environment variable `TOASTTY_UPDATE_SNAPSHOTS=1` to write the
captured output to the golden path instead of comparing — useful when:

- The golden doesn't exist yet (fresh checkout, fresh test).
- An intentional rendering change shifted output.

After regenerating, **inspect the PNG visually** and commit it.

```sh
TOASTTY_UPDATE_SNAPSHOTS=1 cargo test -p toastty-render --test text_snapshot_hello
```

## Determinism

The harness uses the bundled FiraMono font, validation-on adapter, fixed
window size, and the renderer's `Theme::default_dark()`. This is meant
to be reproducible across machines, but SSIM at 0.99 tolerates small
GPU driver / rasterizer drift.
