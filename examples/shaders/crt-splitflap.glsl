// CRT look with a split-flap wave on every bell.
//
// The CRT part is static: barrel curvature, scanlines, an aperture-grille
// mask, slight colour fringing and a vignette. It reads no time input, so
// between bells toastty draws nothing at all.
//
// On each BEL (try `printf '\a'`), the cells flip like the tiles of a
// split-flap departure board. The wave starts at the cell where the bell
// rang and spreads outward until it has crossed the window. Up to four
// overlapping bells (`iBells`) each run their own wave.
//
// Enable it with:
//
//   [shader]
//   paths = ["~/src/toastty/examples/shaders/crt-splitflap.glsl"]

// toastty: animate for this long after an event (bell, cursor move, focus),
// then stop drawing. Must cover SWEEP_SECONDS + FLIP_SECONDS.
const float TOASTTY_EVENT_DURATION = 1.6;

// ---- CRT ----
const float CURVATURE = 0.06;   // barrel distortion, 0 = flat
const float SCANLINES = 0.22;   // scanline darkening, 0..1
const float SCANLINE_PX = 3.0;  // scanline period in pixels
const float MASK = 0.10;        // aperture-grille strength, 0..1
const float FRINGE_PX = 0.6;    // red/blue offset at the edges, px
const float VIGNETTE = 0.30;    // corner darkening, 0..1
const float BRIGHTNESS = 1.12;  // makes up for scanline/mask loss

// ---- split-flap ----
const float SWEEP_SECONDS = 1.0;  // time for the wave to cross the window
const float FLIP_SECONDS = 0.34;  // time for one tile to flip
const float FLAP_SHADE = 0.55;    // darkest the flap gets edge-on
const float SHADOW = 0.35;        // shadow the flap casts on the half below

const float PI = 3.14159265;

// Barrel distortion in 0..1 uv space.
vec2 curve(vec2 uv) {
    vec2 c = uv * 2.0 - 1.0;
    c *= 1.0 + CURVATURE * (c.yx * c.yx);
    return c * 0.5 + 0.5;
}

// Flip phase of the cell centred at `center`: 0..1 while it is flipping,
// -1 when it is at rest. The newest bell that is flipping this cell wins.
float flip_phase(vec2 center) {
    float diag = length(iResolution.xy);
    for (int i = 0; i < 4; i++) {
        vec4 bell = iBells[i];
        float delay = distance(center, bell.xy) / diag * SWEEP_SECONDS;
        float t = (bell.w - delay) / FLIP_SECONDS;
        if (t > 0.0 && t < 1.0) {
            return t;
        }
    }
    return -1.0;
}

// Where to sample for pixel `p` (px), plus a brightness factor. Inside a
// flipping cell this folds the top half down over the hinge at the
// cell's middle, the way a split-flap tile turns.
vec3 split_flap(vec2 p) {
    vec2 grid = p - iGridRect.xy;
    if (any(lessThan(grid, vec2(0.0))) || any(greaterThanEqual(grid, iGridRect.zw))) {
        return vec3(p, 1.0);
    }
    vec2 origin = iGridRect.xy + floor(grid / iCellSize) * iCellSize;
    float t = flip_phase(origin + 0.5 * iCellSize);
    if (t < 0.0) {
        return vec3(p, 1.0);
    }

    vec2 local = (p - origin) / iCellSize;  // 0..1, y down
    float fold = cos(PI * t);               // 1 -> 0 -> -1
    float half_h = 0.5 * abs(fold);         // projected height of the flap
    float light = mix(FLAP_SHADE, 1.0, abs(fold));
    float y = local.y;
    float shade = 1.0;

    if (fold > 0.0) {
        // First half: the top flap tips toward the viewer and shrinks
        // onto the hinge, uncovering the (shadowed) card behind it.
        if (y >= 0.5 - half_h && y < 0.5) {
            y = 0.5 - (0.5 - y) / abs(fold);
            shade = light;
        } else if (y < 0.5) {
            shade = 1.0 - SHADOW * abs(fold);
        }
    } else {
        // Second half: the flap has passed the hinge and now covers the
        // bottom half, unfolding downward. The half it hasn't reached yet
        // sits in its shadow.
        if (y >= 0.5 && y < 0.5 + half_h) {
            y = 0.5 + (y - 0.5) / abs(fold);
            shade = light;
        } else if (y >= 0.5) {
            shade = 1.0 - SHADOW * (1.0 - abs(fold));
        }
    }

    // The hinge and the tile edges show as thin dark gaps mid-flip.
    float px_y = abs(local.y - 0.5) * iCellSize.y;
    vec2 edge = min(local, 1.0 - local) * iCellSize;
    if (px_y < 0.75 || min(edge.x, edge.y) < 0.75) {
        shade *= 0.35;
    }

    return vec3(origin + vec2(local.x, y) * iCellSize, shade);
}

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 uv = curve(fragCoord / iResolution.xy);
    if (any(lessThan(uv, vec2(0.0))) || any(greaterThan(uv, vec2(1.0)))) {
        fragColor = vec4(0.0, 0.0, 0.0, 1.0);  // outside the tube
        return;
    }

    vec3 flap = split_flap(uv * iResolution.xy);
    vec2 src = flap.xy / iResolution.xy;

    // Colour fringing grows toward the edges.
    vec2 fringe = (uv - 0.5) * 2.0 * FRINGE_PX / iResolution.xy;
    vec4 base = texture(iChannel0, src);
    vec3 color = vec3(
        texture(iChannel0, src + fringe).r,
        base.g,
        texture(iChannel0, src - fringe).b
    );
    color *= flap.z;

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
