#version 150

// The smoked-glass plate: a blurred backdrop under a lit material.
//
// `u_tex` is the blurred copy of whatever was already on screen behind this
// rectangle. The rest is the plate itself, built in layers the way a real
// piece of dark glass reads: a tinted body with a slight vertical tone, an
// ambient trace of the accent hue, a directional rim where the edge catches
// the light, an inner reflection along the top, a darker seat at the bottom,
// one broad static reflection band, and grain to keep the field from banding.
//
// Every quantity derives from the rectangle's local coordinates and SDF, so
// the material survives resizing and DPI changes without pixel constants.

in vec2 v_uv;
out vec4 f_color;

uniform sampler2D u_tex;
/// Maps this rectangle's 0..1 into the padded region that was blurred.
uniform vec2 u_uv_scale;
uniform vec2 u_uv_offset;
/// Rectangle size in physical pixels, origin bottom-left.
uniform vec2 u_size;
uniform float u_radius;
/// Straight (not premultiplied) rgba — a is how much of the plate is tint.
uniform vec4 u_tint;
/// x: top inner reflection, y: directional edge reflection,
/// z: inner highlight (thin rim), w: bottom shadow.
uniform vec4 u_mat_a;
/// x: specular width in physical pixels, y: diagonal band strength,
/// z: rose ambient strength, w: whole-plate opacity for fades.
uniform vec4 u_mat_b;
uniform float u_noise;
/// Specular centre in physical pixels; amount of 0 switches it off.
uniform vec2 u_sheen;
uniform float u_sheen_amount;

// The virtual light sits up and to the left; y is up in GL coordinates.
const vec2 LIGHT = vec2(-0.5547, 0.8321);
const vec3 ROSE = vec3(1.0, 0.302, 0.616);
const vec3 EDGE_TINT = vec3(1.0, 0.88, 0.94);

float rounded_box(vec2 p, vec2 half_size, float r) {
    vec2 q = abs(p) - half_size + r;
    return length(max(q, vec2(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

float hash(vec2 p) {
    return fract(sin(dot(p, vec2(127.1, 311.7))) * 43758.5453);
}

void main() {
    vec2 px = v_uv * u_size;
    vec2 centred = px - u_size * 0.5;
    vec2 half_size = u_size * 0.5;
    float d = rounded_box(centred, half_size, u_radius);

    // Derivative coverage follows the physical pixel footprint at straight
    // edges and corners alike. The old fixed, asymmetric interval shrank the
    // plate and shimmered when its point-space rectangle landed between pixels.
    float aa = clamp(fwidth(d), 0.75, 1.5);
    float mask = 1.0 - smoothstep(-0.5 * aa, 0.5 * aa, d);

    // --- body ------------------------------------------------------------
    vec3 col = mix(texture(u_tex, v_uv * u_uv_scale + u_uv_offset).rgb, u_tint.rgb, u_tint.a);

    // Lit from above: without the tonal gradient the plate reads flat no
    // matter how good the blur behind it is.
    col *= mix(0.90, 1.06, v_uv.y);

    // A trace of the accent, as if the pink in the UI reflected back into the
    // glass. Slightly stronger toward the lit top.
    col += ROSE * (u_mat_b.z * (0.4 + 0.6 * v_uv.y));

    // --- edge normal from the SDF ----------------------------------------
    // Outward direction toward the nearest edge/corner. Degenerate deep in
    // the interior, where the rim terms are zero anyway.
    vec2 q = abs(centred) - half_size + u_radius;
    vec2 n = normalize(max(q, vec2(0.0)) + vec2(1e-4)) * sign(centred);
    float facing = max(dot(n, LIGHT), 0.0);
    float directional = pow(facing, 2.4);

    // Two rim frequencies: a thin crisp line at the edge and a soft halo
    // behind it, both fading with distance from the border.
    float w = max(u_mat_b.x, 0.5);
    float rim = exp(-abs(d) / w) * 0.7 + exp(-abs(d) / (w * 3.5)) * 0.3;

    // Perfectly uniform reflection looks synthetic; a slow drift breaks it.
    float breakup = 0.88 + 0.12 * sin(dot(px, vec2(0.011, 0.005)) + sin(px.x * 0.002) * 2.1);

    col += EDGE_TINT * (rim * directional * u_mat_a.y * breakup);

    // The thin inner highlight the old bevel had, top-weighted.
    col += vec3(smoothstep(2.0, 0.0, -d) * u_mat_a.z * (0.25 + 0.75 * v_uv.y));

    // --- inner top reflection / bottom seat -------------------------------
    // A band of light hanging just under the top edge, wandering slowly
    // across the width so it does not read as a ruled line.
    float top_band = smoothstep(0.80, 0.99, v_uv.y) * (0.78 + 0.22 * cos(v_uv.x * 2.9 - 0.7));
    col += EDGE_TINT * (top_band * u_mat_a.x);

    // The glass sits darker into its seat.
    col *= 1.0 - smoothstep(0.25, 0.0, v_uv.y) * u_mat_a.w;

    // --- one broad, static reflection band --------------------------------
    // Travels upper-left to lower-right through local coordinates.
    float t = dot(v_uv - vec2(0.22, 1.0), vec2(0.5547, -0.8321));
    float band = exp(-t * t / 0.05) * (0.9 + 0.1 * sin(v_uv.x * 6.3));
    col += EDGE_TINT * (band * u_mat_b.y);

    // --- pointer sheen ----------------------------------------------------
    if (u_sheen_amount > 0.0) {
        float reach = 0.85 * max(u_size.x, u_size.y);
        float s = 1.0 - clamp(distance(px, u_sheen) / reach, 0.0, 1.0);
        col += vec3(s * s * u_sheen_amount);
    }

    col += vec3(hash(px) - 0.5) * u_noise;

    // egui blends premultiplied, so a fading plate scales all four channels.
    f_color = vec4(col * mask, mask) * u_mat_b.w;
}
