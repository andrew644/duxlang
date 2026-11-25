typedef struct Point {
    int x, y;
} Point;

typedef struct Pair {
    float a, b;
} Pair;

Point make_point(int x, int y) {
    Point p;
    p.x = x;
    p.y = y;
    return p;
}

int sum_point(Point p) {
    return p.x + p.y;
}

Point shift_point(Point p, int by) {
    p.x += by;
    p.y += by;
    return p;
}

Pair halves(int whole) {
    Pair r;
    r.a = (float)whole / 2.0f;
    r.b = (float)whole / 4.0f;
    return r;
}
