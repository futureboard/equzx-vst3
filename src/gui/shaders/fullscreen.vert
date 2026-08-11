#version 150

// A single oversized triangle, generated from the vertex index. No vertex
// buffer, no attributes — just three vertices that cover the viewport, which is
// all any of these post passes needs.
out vec2 v_uv;

void main() {
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    v_uv = p;
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}
