typedef unsigned long dux_size;
typedef long time_like;

struct point {
    int x;
    int y;
};

struct rect {
    struct point top;
    struct point bottom;
};

struct bits {
    unsigned a : 3;
    unsigned b : 5;
};

union score {
    int whole;
    double part;
};

struct hidden;
typedef struct hidden hidden_t;

enum color {
    RED,
    GREEN = 7,
    BLUE
};

int dux_add(int a, int b);
char *dux_name(void);
double dux_sum(int count, ...);
void dux_hide(hidden_t *what);
static inline int dux_twice(int x) { return x + x; }
extern int dux_counter;

#define DUX_MAX 64
#define DUX_RATIO 1.5
#define DUX_TAG "dux"
#define DUX_PAGES (4 * 1024)
#define DUX_DOUBLED(x) ((x) * 2)

int dux_anon(int, int);

union counter {
    struct {
        int low;
        int high;
    };
    long whole;
};
