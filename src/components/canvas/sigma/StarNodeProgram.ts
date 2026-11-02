import { NodeCircleProgram } from 'sigma/rendering';

// Keep Sigma's circular picking area so the points and gaps of a small star
// are equally easy to click. Only the visible fragment uses the star outline.
const STAR_FRAGMENT_SHADER = `
precision highp float;

varying vec4 v_color;
varying vec2 v_diffVector;
varying float v_radius;

uniform float u_correctionRatio;

const float PI = 3.14159265359;
const float STEP = PI / 5.0;

float cross2(vec2 a, vec2 b) {
  return a.x * b.y - a.y * b.x;
}

void main(void) {
  float distanceFromCenter = length(v_diffVector);

  #ifdef PICKING_MODE
  if (distanceFromCenter > v_radius) discard;
  gl_FragColor = v_color;
  #else
  float angle = mod(atan(v_diffVector.y, v_diffVector.x) - PI / 2.0 + 2.0 * PI, 2.0 * PI);
  float segment = floor(angle / STEP);
  float firstRadius = mod(segment, 2.0) < 1.0 ? v_radius : v_radius * 0.45;
  float nextRadius = mod(segment, 2.0) < 1.0 ? v_radius * 0.45 : v_radius;
  float firstAngle = segment * STEP + PI / 2.0;
  vec2 first = firstRadius * vec2(cos(firstAngle), sin(firstAngle));
  vec2 next = nextRadius * vec2(cos(firstAngle + STEP), sin(firstAngle + STEP));
  vec2 ray = vec2(cos(angle + PI / 2.0), sin(angle + PI / 2.0));
  vec2 edge = next - first;
  float boundary = cross2(first, edge) / cross2(ray, edge);
  float coverage = clamp((boundary - distanceFromCenter) / (u_correctionRatio * 2.0), 0.0, 1.0);
  gl_FragColor = mix(vec4(0.0), v_color, coverage);
  #endif
}
`;

export default class StarNodeProgram extends NodeCircleProgram {
  getDefinition() {
    return { ...super.getDefinition(), FRAGMENT_SHADER_SOURCE: STAR_FRAGMENT_SHADER };
  }
}
