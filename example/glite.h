// The definition behind the example's hand-written declaration. A header
// the program names is compiled into the program, so a symbol can live
// here — no library to build, nothing for the user to install. The
// `#[c_name("glm_vec3_scale")]` extern finds this definition in the
// program's own bitcode.
#ifndef GLITE_H
#define GLITE_H

typedef struct {
    float x;
    float y;
    float z;
} glite_vec3;

void glm_vec3_scale(glite_vec3 *v, float s) {
    v->x *= s;
    v->y *= s;
    v->z *= s;
}

#endif
