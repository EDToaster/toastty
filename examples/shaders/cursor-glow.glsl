// A soft, warm yellow glow around the cursor.
//
// Static: it reads only the cursor uniforms, which change only when the
// cursor moves (and that redraws anyway), so it costs nothing at idle.
// The glow follows the cursor's visibility, so it blinks with a blinking
// cursor and disappears when an app hides the cursor.
//
//   [shader]
//   paths = ["shaders/cursor-glow.glsl"]

const vec3 GLOW_COLOR = vec3(1.0, 0.78, 0.38);  // warm yellow
const float STRENGTH = 0.16;    // peak brightness next to the cursor, 0..1
const float REACH_CELLS = 2.2;  // how far the glow fades out, in cell heights

// Distance from `p` to the rectangle with corners `lo`, `hi` (0 inside).
float rect_distance(vec2 p, vec2 lo, vec2 hi) {
    vec2 d = max(max(lo - p, p - hi), 0.0);
    return length(d);
}

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec4 base = texture(iChannel0, fragCoord / iResolution.xy);
    if (iCursorVisible == 0) {
        fragColor = base;
        return;
    }

    // iCurrentCursor: xy = left, bottom edge (y points down); zw = size.
    vec2 lo = vec2(iCurrentCursor.x, iCurrentCursor.y - iCurrentCursor.w);
    vec2 hi = vec2(iCurrentCursor.x + iCurrentCursor.z, iCurrentCursor.y);
    float d = rect_distance(fragCoord, lo, hi);

    // Smooth falloff to zero at the reach, and none on the cursor itself
    // so the character under it keeps its colours.
    float reach = REACH_CELLS * iCellSize.y;
    float falloff = 1.0 - smoothstep(0.0, reach, d);
    float g = STRENGTH * falloff * falloff * smoothstep(0.0, 1.5, d);

    // Screen blend in premultiplied space: brightens without clipping,
    // and stays a valid premultiplied colour over a transparent background.
    vec4 glow = vec4(GLOW_COLOR * g, g);
    fragColor = base + glow - base * glow;
}
