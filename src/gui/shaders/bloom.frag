#version 150

// Additive composite of the bright-passed, blurred copy of the plot.
//
// This is what replaces the canvas `shadowBlur` the composite EQ curve used to
// be drawn with. Because it runs over the framebuffer rather than around a
// path, the glow picks up the analyser peaks and the band handles as well —
// which is what a real bloom does, and what the old one could not.

in vec2 v_uv;
out vec4 f_color;

uniform sampler2D u_tex;
uniform vec2 u_uv_scale;
uniform vec2 u_uv_offset;
uniform vec3 u_tint;
uniform float u_intensity;

void main() {
    vec3 c = texture(u_tex, v_uv * u_uv_scale + u_uv_offset).rgb;
    // Alpha stays at zero: with egui's premultiplied blend that leaves the
    // destination alone and simply adds light to it.
    f_color = vec4(c * u_tint * u_intensity, 0.0);
}
