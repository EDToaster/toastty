// The cursor morphs from its old position to its new one.
//
// When the cursor jumps, a smear in the cursor's colour stretches from
// where it was to where it is. The leading edge races ahead and the
// trailing edge catches up, so the shape stretches out mid-flight, then
// shrinks into the cursor at its new cell and fades away. It is the
// convex hull of two copies of the cursor rect that move along the path
// at different speeds.
//
// It animates for MORPH_SECONDS after each cursor change and is idle
// otherwise. The real cursor is never covered: the smear is drawn only
// outside the current cursor rect.
//
//   [shader]
//   paths = ["shaders/cursor-morph.glsl"]

// toastty: animate for this long after an event (bell, cursor move, focus),
// then stop drawing. Must cover MORPH_SECONDS.
const float TOASTTY_EVENT_DURATION = 0.3;

const float MORPH_SECONDS = 0.22;  // how long the morph lasts
const float FADE_FRACTION = 0.35;  // the last part of the morph fades out
const float OPACITY       = 0.85;  // peak opacity of the smear, 0..1

// Centre of a cursor rect (`xy` = left, bottom edge; `zw` = size; y down).
vec2 centre(vec4 c) {
    return vec2(c.x + 0.5 * c.z, c.y - 0.5 * c.w);
}

// Signed distance (px) from `p` to the hull of the box of half-size `h`
// as its centre slides from `a` to `b`. The distance to the box is convex
// in the slide parameter, so a ternary search finds the closest position.
float hull_distance(vec2 p, vec2 a, vec2 b, vec2 h) {
    float lo = 0.0;
    float hi = 1.0;
    for (int i = 0; i < 24; i++) {
        float m1 = mix(lo, hi, 1.0 / 3.0);
        float m2 = mix(lo, hi, 2.0 / 3.0);
        vec2 q1 = abs(p - mix(a, b, m1)) - h;
        vec2 q2 = abs(p - mix(a, b, m2)) - h;
        if (max(q1.x, q1.y) < max(q2.x, q2.y)) {
            hi = m2;
        } else {
            lo = m1;
        }
    }
    vec2 q = abs(p - mix(a, b, 0.5 * (lo + hi))) - h;
    return max(q.x, q.y);
}

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec4 base = texture(iChannel0, fragCoord / iResolution.xy);
    float t = (iTime - iTimeCursorChange) / MORPH_SECONDS;
    if (iCursorVisible == 0 || t < 0.0 || t >= 1.0) {
        fragColor = base;
        return;
    }

    vec2 from = centre(iPreviousCursor);
    vec2 to = centre(iCurrentCursor);
    vec2 half_size = 0.5 * iCurrentCursor.zw;

    // The head eases out (fast start), the tail eases in-out (lags behind).
    float head = 1.0 - pow(1.0 - t, 3.0);
    float tail = t * t * (3.0 - 2.0 * t);
    vec2 a = mix(from, to, tail);
    vec2 b = mix(from, to, head);

    float sd = hull_distance(fragCoord, a, b, half_size);
    float cover = clamp(0.5 - sd, 0.0, 1.0);

    // Leave the real cursor (and the character under it) alone.
    vec2 q = abs(fragCoord - to) - half_size;
    cover *= clamp(0.5 + max(q.x, q.y), 0.0, 1.0);

    float fade = 1.0 - smoothstep(1.0 - FADE_FRACTION, 1.0, t);
    float alpha = cover * fade * OPACITY * iCurrentCursorColor.a;

    // Source-over in premultiplied space.
    vec4 smear = vec4(iCurrentCursorColor.rgb * alpha, alpha);
    fragColor = smear + base * (1.0 - alpha);
}
