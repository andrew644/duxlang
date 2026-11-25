static inline int cedge_half(int x) {
    return x / 2;
}

static inline int cedge_service(int x) {
    return cedge_half(x) + 41;
}

static int cedge_visits = 0;

static inline int cedge_visit(void) {
    cedge_visits += 1;
    return cedge_visits;
}
