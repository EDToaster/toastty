// CRT look with a split-flap wave on every bell.
//
// The CRT part is static: barrel curvature, scanlines, an aperture-grille
// mask, slight colour fringing and a vignette. It reads no time input, so
// between bells toastty draws nothing at all.
//
// On each BEL (try `printf '\a'`), the cells flip like the tiles of a
// split-flap departure board: each tile riffles through two characters
// borrowed from elsewhere on screen, then lands back on its own. The wave
// starts at the cell where the bell rang and spreads outward until it has
// crossed the window. Up to four overlapping bells (`iBells`) each run
// their own wave.
//
// Enable it with:
//
//   [shader]
//   paths = ["~/src/toastty/examples/shaders/crt-splitflap.glsl"]

// toastty: animate for this long after an event (bell, cursor move, focus),
// then stop drawing. Must cover SWEEP_SECONDS + FLIPS * FLIP_SECONDS.
const float TOASTTY_EVENT_DURATION = 1.6;

// ---- CRT ----
const float CURVATURE   = 0.02;   // barrel distortion, 0 = flat
const float SCANLINES   = 0.12;   // scanline darkening, 0..1
const float SCANLINE_PX = 2.0;    // scanline period in pixels
const float MASK        = 0.10;   // aperture-grille strength, 0..1
const float FRINGE_PX   = 0.6;    // red/blue offset at the edges, px
const float VIGNETTE    = 0.30;   // corner darkening, 0..1
const float BRIGHTNESS  = 1.54;   // makes up for scanline/mask loss

// ---- split-flap ----
const float SWEEP_SECONDS = 2.0;  // time for the wave to cross the window
const int FLIPS           = 2;    // flips per tile: two random characters, then home
const float FLIP_SECONDS  = 0.2;  // time for one flip
const float FLAP_SHADE    = 0.55; // darkest the flap gets edge-on
const float SHADOW        = 0.45; // shadow the flap casts on the card behind it
const float TILE_LIFT     = 0.07; // flipping tiles are drawn as lighter cards

const float PI = 3.14159265;

// Barrel distortion in 0..1 uv space.
vec2 curve(vec2 uv) {
    vec2 c = uv * 2.0 - 1.0;
    c *= 1.0 + CURVATURE * (c.yx * c.yx);
    return c * 0.5 + 0.5;
}

// Dave Hoskins' hash: no `sin`, so it stays random for large inputs.
float hash(vec2 p) {
    vec3 p3 = fract(vec3(p.xyx) * 0.1031);
    p3 += dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

// How far the tile centred at `center` is through its flips (0..FLIPS),
// or -1 when it is at rest. `y` is a per-bell seed. The newest bell that
// is flipping this tile wins.
vec2 flip_progress(vec2 center) {
    float diag = length(iResolution.xy);
    for (int i = 0; i < 4; i++) {
        vec4 bell = iBells[i];
        float delay = distance(center, bell.xy) / diag * SWEEP_SECONDS;
        float p = (bell.w - delay) / FLIP_SECONDS;
        if (p > 0.0 && p < float(FLIPS)) {
            return vec2(p, mod(bell.z * 61.0, 997.0));
        }
    }
    return vec2(-1.0, 0.0);
}

// How much a cell differs from the background: a few taps across it.
float ink(vec2 cell) {
    vec2 o = iGridRect.xy + cell * iCellSize;
    float sum = 0.0;
    for (int i = 0; i < 3; i++) {
        vec2 at = o + iCellSize * vec2(0.5, 0.3 + 0.2 * float(i));
        sum += dot(abs(texture(iChannel0, at / iResolution.xy).rgb - iBackgroundColor), vec3(1.0));
    }
    return sum;
}

// The cell whose character card `k` of tile `cell` shows. Card 0 is the
// tile's own character and so is the last one, so every tile comes home.
// The cards in between borrow a random character from elsewhere on
// screen, skipping (up to a few) blank ones.
vec2 card(vec2 cell, int k, float seed) {
    if (k <= 0 || k >= FLIPS) {
        return cell;
    }
    vec2 cells = floor(iGridRect.zw / iCellSize);
    vec2 cand = cell;
    for (int j = 0; j < 6; j++) {
        // Retries step far apart: with `+ j`, tile x's second try would be
        // tile x+1's first, and neighbours would settle on the same cell.
        vec2 h = vec2(float(k * 6 + j) * 211.0, seed);
        cand = floor(vec2(hash(cell + h), hash(cell.yx + h.yx + 17.0)) * cells);
        if (ink(cand) > 0.25) {
            break;
        }
    }
    return cand;
}

// Where to sample for pixel `p` (px): `xy` = source pixel, `z` = shade,
// `w` = 1 inside a flipping tile. A flip is the physical one: the top
// flap of the old card falls toward the viewer over the hinge in the
// middle, uncovering the new card's top half, and its back (the new
// card's bottom half) unfolds over the old bottom half.
vec4 split_flap(vec2 p) {
    vec2 grid = p - iGridRect.xy;
    if (any(lessThan(grid, vec2(0.0))) || any(greaterThanEqual(grid, iGridRect.zw))) {
        return vec4(p, 1.0, 0.0);
    }
    vec2 cell = floor(grid / iCellSize);
    vec2 origin = iGridRect.xy + cell * iCellSize;
    vec2 progress = flip_progress(origin + 0.5 * iCellSize);
    if (progress.x < 0.0) {
        return vec4(p, 1.0, 0.0);
    }

    int k = int(progress.x);
    float t = fract(progress.x);
    vec2 old_card = card(cell, k, progress.y);
    vec2 new_card = card(cell, k + 1, progress.y);

    vec2 local = (p - origin) / iCellSize;  // 0..1, y down
    float fold = cos(PI * t);               // 1 -> 0 -> -1
    float half_h = 0.5 * abs(fold);         // projected height of the flap
    float light = mix(FLAP_SHADE, 1.0, abs(fold));
    vec2 from = new_card;
    float y = local.y;
    float shade = 1.0;

    if (fold > 0.0) {
        if (y < 0.5 - half_h) {
            shade = 1.0 - SHADOW * abs(fold);   // new top, in the flap's shadow
        } else if (y < 0.5) {
            from = old_card;                    // the falling flap
            y = 0.5 - (0.5 - y) / abs(fold);
            shade = light;
        } else {
            from = old_card;                    // old bottom, not covered yet
        }
    } else {
        if (y < 0.5) {
            // new top, settled
        } else if (y < 0.5 + half_h) {
            y = 0.5 + (y - 0.5) / abs(fold);    // the flap's back unfolding
            shade = light;
        } else {
            from = old_card;                    // old bottom, in shadow
            shade = 1.0 - SHADOW * (1.0 - abs(fold));
        }
    }

    // The hinge and the gaps between tiles.
    float hinge = abs(local.y - 0.5) * iCellSize.y;
    vec2 edge = min(local, 1.0 - local) * iCellSize;
    if (hinge < 0.75 || min(edge.x, edge.y) < 0.75) {
        shade *= 0.3;
    }

    vec2 src = iGridRect.xy + (from + vec2(local.x, y)) * iCellSize;
    return vec4(src, shade, 1.0);
}

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 uv = curve(fragCoord / iResolution.xy);
    if (any(lessThan(uv, vec2(0.0))) || any(greaterThan(uv, vec2(1.0)))) {
        fragColor = vec4(0.0, 0.0, 0.0, 1.0);  // outside the tube
        return;
    }

    vec4 flap = split_flap(uv * iResolution.xy);
    vec2 src = flap.xy / iResolution.xy;

    // Colour fringing grows toward the edges.
    vec2 fringe = (uv - 0.5) * 2.0 * FRINGE_PX / iResolution.xy;
    vec4 base = texture(iChannel0, src);
    vec3 color = vec3(
        texture(iChannel0, src + fringe).r,
        base.g,
        texture(iChannel0, src - fringe).b
    );
    color = (color + flap.w * TILE_LIFT) * flap.z;

    // Scanlines follow the curved surface, so they bend with it.
    float scan = 0.5 + 0.5 * cos(2.0 * PI * uv.y * iResolution.y / SCANLINE_PX);
    color *= 1.0 - SCANLINES * scan;

    // Aperture grille: one bright channel per pixel column, in screen space.
    int column = int(mod(floor(fragCoord.x), 3.0));
    vec3 mask = vec3(1.0 - MASK);
    mask[column] = 1.0;
    color *= mask;

    float v = 16.0 * uv.x * uv.y * (1.0 - uv.x) * (1.0 - uv.y);
    color *= mix(1.0 - VIGNETTE, 1.0, pow(v, 0.3));

    // `base.a` keeps a transparent background transparent; the colour is
    // only ever darkened, so it stays a valid premultiplied value.
    fragColor = vec4(min(color * BRIGHTNESS, vec3(base.a)), base.a);
}
