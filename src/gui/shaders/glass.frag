#version 150

// The frosted plate: what the old stylesheet asked the browser for with
// `backdrop-filter: blur()`, done properly.
//
// `u_tex` is the blurred copy of whatever was already on screen behind this
// rectangle. Everything else here is the plate itself — the tint it holds, the
// light it catches along its top edge, the specular the pointer drags across
// it, and the grain that stops a large flat panel from banding.

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
uniform float u_rim;
uniform float u_noise;
/// Specular centre in physical pixels; amount of 0 switches it off.
uniform vec2 u_sheen;
uniform float u_sheen_amount;

float rounded_box(vec2 p, vec2 half_size, float r) {
    vec2 q = abs(p) - half_size + r;
    return length(max(q, vec2(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

float hash(vec2 p) {
    return fract(sin(dot(p, vec2(127.1, 311.7))) * 43758.5453);
}

void main() {
    vec2 px = v_uv * u_size;
    float d = rounded_box(px - u_size * 0.5, u_size * 0.5, u_radius);

    // One pixel of feather, so the corners are as smooth as egui's own.
    float mask = 1.0 - smoothstep(-1.0, 0.5, d);
    if (mask <= 0.002) {
        discard;
    }

    vec3 col = mix(texture(u_tex, v_uv * u_uv_scale + u_uv_offset).rgb, u_tint.rgb, u_tint.a);

    // Lit from above: without the gradient the plate reads as a flat rectangle
    // of grey no matter how good the blur behind it is.
    col *= mix(0.93, 1.05, v_uv.y);

    // Inner rim, strongest along the top edge where a real bevel would catch.
    col += vec3(smoothstep(2.0, 0.0, -d) * u_rim * (0.3 + 0.7 * v_uv.y));

    if (u_sheen_amount > 0.0) {
        float reach = 0.85 * max(u_size.x, u_size.y);
        float s = 1.0 - clamp(distance(px, u_sheen) / reach, 0.0, 1.0);
        col += vec3(s * s * u_sheen_amount);
    }

    col += vec3(hash(px) - 0.5) * u_noise;

    // egui blends premultiplied.
    f_color = vec4(col * mask, mask);
}
