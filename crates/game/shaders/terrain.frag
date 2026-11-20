#version 450

layout(location = 0) in vec3 v_color;
layout(location = 1) in float v_dist;

layout(push_constant) uniform Push {
    mat4 view_proj;
    vec4 offset_fog;
    vec4 fog_color;
} pc;

layout(location = 0) out vec4 out_color;

void main() {
    // Fade into the sky colour over the last 30% of the view distance so the
    // edge of the loaded area isn't a hard line.
    float fog_end = pc.offset_fog.w;
    float f = clamp((v_dist - fog_end * 0.7) / (fog_end * 0.3), 0.0, 1.0);
    out_color = vec4(mix(v_color, pc.fog_color.rgb, f), 1.0);
}
