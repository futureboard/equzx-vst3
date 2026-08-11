#version 150

// Dual-Kawase upsample: the tent-weighted eight-tap that pairs with `down.frag`.
// Running it once per level on the way back up is what turns two cheap
// downsamples into something that looks like a wide Gaussian.

in vec2 v_uv;
out vec4 f_color;

uniform sampler2D u_tex;
/// One texel of the source, scaled by the caller to widen or tighten the blur.
uniform vec2 u_texel;

void main() {
    vec2 h = u_texel;
    vec4 sum = texture(u_tex, v_uv + vec2(-h.x * 2.0, 0.0));
    sum += texture(u_tex, v_uv + vec2(-h.x,  h.y)) * 2.0;
    sum += texture(u_tex, v_uv + vec2( 0.0,  h.y * 2.0));
    sum += texture(u_tex, v_uv + vec2( h.x,  h.y)) * 2.0;
    sum += texture(u_tex, v_uv + vec2( h.x * 2.0, 0.0));
    sum += texture(u_tex, v_uv + vec2( h.x, -h.y)) * 2.0;
    sum += texture(u_tex, v_uv + vec2( 0.0, -h.y * 2.0));
    sum += texture(u_tex, v_uv + vec2(-h.x, -h.y)) * 2.0;
    f_color = sum / 12.0;
}
