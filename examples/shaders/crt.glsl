// CRT look: barrel curvature, scanlines, an aperture-grille mask, slight
// colour fringing and a vignette.
//
// It is static and reads no time input, so it draws nothing between
// events. Combine it with splitflap.glsl (listed first) for a CRT that
// flips its tiles on every bell:
//
//   [shader]
//   paths = ["shaders/crt.glsl"]

const float CURVATURE   = 0.02;   // barrel distortion, 0 = flat
const float SCANLINES   = 0.12;   // scanline darkening, 0..1
const float SCANLINE_PX = 2.0;    // scanline period in pixels
const float MASK        = 0.10;   // aperture-grille strength, 0..1
const float FRINGE_PX   = 0.6;    // red/blue offset at the edges, px
const float VIGNETTE    = 0.30;   // corner darkening, 0..1
const float BRIGHTNESS  = 1.54;   // makes up for scanline/mask loss

const float PI = 3.14159265;

// Barrel distortion in 0..1 uv space.
vec2 curve(vec2 uv) {
    vec2 c = uv * 2.0 - 1.0;
    c *= 1.0 + CURVATURE * (c.yx * c.yx);
    return c * 0.5 + 0.5;
}

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 uv = curve(fragCoord / iResolution.xy);
    if (any(lessThan(uv, vec2(0.0))) || any(greaterThan(uv, vec2(1.0)))) {
        fragColor = vec4(0.0, 0.0, 0.0, 1.0);  // outside the tube
        return;
    }

    // Colour fringing grows toward the edges.
    vec2 fringe = (uv - 0.5) * 2.0 * FRINGE_PX / iResolution.xy;
    vec4 base = texture(iChannel0, uv);
    vec3 color = vec3(
        texture(iChannel0, uv + fringe).r,
        base.g,
        texture(iChannel0, uv - fringe).b
    );

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
