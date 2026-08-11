#version 150

// Dual-Kawase downsample. Five taps at half resolution, which is what makes the
// whole blur cheap: two of these take a panel-sized region down to a sixteenth
// of its pixels before anything wide is convolved over it.
//
// The same program doubles as the bright pass for the bloom — the extraction is
// one smoothstep, and folding it in here means the brightest pixels are found
// while they are still at full resolution.

in vec2 v_uv;
out vec4 f_color;

uniform sampler2D u_tex;
/// One texel of the *source*, so the taps land between its pixels.
uniform vec2 u_texel;
/// 0 for a plain downsample, 1 to keep only what is brighter than the threshold.
uniform float u_bright;
uniform float u_threshold;

void main() {
    vec4 sum = texture(u_tex, v_uv) * 4.0;
    sum += texture(u_tex, v_uv + vec2(-u_texel.x, -u_texel.y));
    sum += texture(u_tex, v_uv + vec2( u_texel.x, -u_texel.y));
    sum += texture(u_tex, v_uv + vec2(-u_texel.x,  u_texel.y));
    sum += texture(u_tex, v_uv + vec2( u_texel.x,  u_texel.y));
    vec4 c = sum * 0.125;

    if (u_bright > 0.5) {
        // Value rather than luminance: the curve this is lifting is a saturated
        // pink, and a luminance weighting would throw most of it away.
        float v = max(max(c.r, c.g), c.b);
        c.rgb *= smoothstep(u_threshold, min(u_threshold + 0.3, 1.0), v);
    }

    f_color = c;
}
